use super::*;
use crate::account_registry::global::runtime::GlobalAccountCatalog;
use crate::account_registry::global::token_manager::RawMeter;
use crate::account_registry::global::token_manager::RawRateLimit;
use crate::account_registry::global::token_manager::RawSnapshot;
use pretty_assertions::assert_eq;

const NOW: i64 = 10_000;

fn account_id(number: u32) -> AccountId {
    AccountId::parse(&format!("C{number}")).unwrap()
}

fn meter(id: &str, utilization: f64, reset_at: i64, observed_at: i64) -> RawMeter {
    RawMeter {
        id: id.to_string(),
        label: id.to_uppercase(),
        utilization,
        reset_at,
        observed_at,
        utilization_observed_at: observed_at,
        state: "normal".to_string(),
    }
}

fn snapshot(
    label: &str,
    fetched_at: i64,
    source_ref: &str,
    utilization: f64,
    reset_at: i64,
) -> RawSnapshot {
    RawSnapshot {
        label: label.to_string(),
        provider_type: "codex-chatgpt".to_string(),
        source_ref: Some(source_ref.to_string()),
        fetched_at,
        ok: true,
        rate_limit: Some(RawRateLimit {
            meters: vec![meter("7d", utilization, reset_at, fetched_at)],
            status: "allowed".to_string(),
        }),
    }
}

fn readiness(ids: &[u32]) -> Vec<CredentialReadiness> {
    ids.iter()
        .map(|number| CredentialReadiness {
            account_id: account_id(*number),
            ready: true,
        })
        .collect()
}

fn selected_id(selection: CatalogSelection) -> Option<AccountId> {
    match selection {
        CatalogSelection::Selected(token) => Some(token.account_id),
        CatalogSelection::Unavailable => None,
    }
}

#[test]
fn fixed_uses_matching_identity_and_auth_without_quota_freshness_gate() {
    let catalog = GlobalAccountCatalog::default();
    let mut exhausted = snapshot(
        "C1",
        NOW - FRESHNESS_SECONDS - 1,
        "identity",
        1.0,
        NOW + 100,
    );
    exhausted.ok = false;
    exhausted.rate_limit.as_mut().unwrap().status = "rejected".to_string();
    catalog.replace(vec![exhausted]).unwrap();
    let ready = readiness(&[1]);
    let CatalogSelection::Selected(token) = catalog.select(CatalogSelectionRequest {
        mode: RotationMode::Fixed,
        fixed_account_id: AccountId::parse("C1"),
        automatic_account_ids: &[],
        current_account_id: None,
        last_committed_account_id: None,
        credential_readiness: &ready,
        now: NOW,
    }) else {
        panic!("fixed selection failed");
    };
    assert_eq!(token.account_id, account_id(1));
    assert_eq!(token.generation, 1);
    assert_eq!(token.source_ref(), "identity");
    assert!(!format!("{token:?}").contains("identity"));
}

#[test]
fn quota_aware_uses_bottleneck_ratio_and_natural_tie_break() {
    let catalog = GlobalAccountCatalog::default();
    let mut c1 = snapshot("C1", NOW, "one", 0.2, NOW + 100);
    c1.rate_limit
        .as_mut()
        .unwrap()
        .meters
        .push(meter("5h", 0.8, NOW + 100, NOW));
    let c2 = snapshot("C2", NOW, "two", 0.2, NOW + 200);
    let c3 = snapshot("C3", NOW, "three", 0.2, NOW + 200);
    catalog.replace(vec![c3, c1, c2]).unwrap();
    let ready = readiness(&[1, 2, 3]);
    let membership = [account_id(1), account_id(2), account_id(3)];
    assert_eq!(
        selected_id(catalog.select(CatalogSelectionRequest {
            mode: RotationMode::QuotaAware,
            fixed_account_id: None,
            automatic_account_ids: &membership,
            current_account_id: None,
            last_committed_account_id: None,
            credential_readiness: &ready,
            now: NOW,
        })),
        Some(account_id(2))
    );
}

#[test]
fn quota_aware_rejects_stale_utilization() {
    let catalog = GlobalAccountCatalog::default();
    let mut stale = snapshot("C1", NOW, "one", 0.2, NOW + 100);
    stale.rate_limit.as_mut().unwrap().meters[0].utilization_observed_at =
        NOW - FRESHNESS_SECONDS - 1;
    catalog.replace(vec![stale]).unwrap();
    let ready = readiness(&[1]);
    assert_eq!(
        catalog.select(CatalogSelectionRequest {
            mode: RotationMode::QuotaAware,
            fixed_account_id: None,
            automatic_account_ids: &[account_id(1)],
            current_account_id: None,
            last_committed_account_id: None,
            credential_readiness: &ready,
            now: NOW,
        }),
        CatalogSelection::Unavailable
    );
}

#[test]
fn sequential_modes_respect_membership_and_skip_exhausted_accounts() {
    let catalog = GlobalAccountCatalog::default();
    let c1 = snapshot("C1", NOW, "one", 0.1, NOW + 100);
    let mut c2 = snapshot("C2", NOW, "two", 1.0, NOW + 100);
    c2.rate_limit.as_mut().unwrap().meters[0].state = "exhausted".to_string();
    let c3 = snapshot("C3", NOW, "three", 0.1, NOW + 100);
    catalog.replace(vec![c1, c2, c3]).unwrap();
    let ready = readiness(&[1, 2, 3]);
    let membership = [account_id(1), account_id(2), account_id(3)];

    let select = |mode, anchor| {
        selected_id(catalog.select(CatalogSelectionRequest {
            mode,
            fixed_account_id: None,
            automatic_account_ids: &membership,
            current_account_id: anchor,
            last_committed_account_id: None,
            credential_readiness: &ready,
            now: NOW,
        }))
    };
    assert_eq!(
        select(RotationMode::RoundRobin, Some(account_id(1))),
        Some(account_id(2))
    );
    assert_eq!(
        select(RotationMode::ExhaustThenNext, Some(account_id(1))),
        Some(account_id(1))
    );
    assert_eq!(
        select(RotationMode::ExhaustThenNext, Some(account_id(2))),
        Some(account_id(3))
    );
}

#[test]
fn exhaust_then_next_does_not_reuse_expired_exhaustion_evidence() {
    let catalog = GlobalAccountCatalog::default();
    let mut reset = snapshot("C1", NOW, "one", 1.0, NOW - 1);
    reset.rate_limit.as_mut().unwrap().meters[0].state = "exhausted".to_string();
    catalog.replace(vec![reset]).unwrap();
    let ready = readiness(&[1]);

    assert_eq!(
        selected_id(catalog.select(CatalogSelectionRequest {
            mode: RotationMode::ExhaustThenNext,
            fixed_account_id: None,
            automatic_account_ids: &[account_id(1)],
            current_account_id: Some(account_id(1)),
            last_committed_account_id: None,
            credential_readiness: &ready,
            now: NOW,
        })),
        Some(account_id(1))
    );
}
