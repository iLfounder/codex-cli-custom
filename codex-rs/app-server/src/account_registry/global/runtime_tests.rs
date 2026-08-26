use super::*;
use crate::account_registry::global::token_manager::RawMeter;
use crate::account_registry::global::token_manager::RawRateLimit;
use crate::account_registry::global::token_manager::RawSnapshot;
use pretty_assertions::assert_eq;

const NOW: i64 = 10_000;

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

#[test]
fn full_replace_filters_and_naturally_orders_without_generation_churn() {
    let catalog = GlobalAccountCatalog::default();
    let mut ignored = snapshot("A1", NOW, "ignored", 0.1, NOW + 100);
    ignored.provider_type = "claude-oauth".to_string();
    let accounts = vec![
        snapshot("C10", NOW, "ten", 0.2, NOW + 100),
        ignored,
        snapshot("C2", NOW, "two", 0.3, NOW + 100),
    ];
    assert_eq!(
        catalog.replace(accounts.clone()).unwrap(),
        ApplyOutcome::Applied { generation: 1 }
    );
    assert_eq!(
        catalog.replace(accounts).unwrap(),
        ApplyOutcome::Unchanged { generation: 1 }
    );
    assert_eq!(
        catalog
            .projection(NOW)
            .into_iter()
            .map(|account| account.account_slot_id)
            .collect::<Vec<_>>(),
        vec!["C2", "C10"]
    );
}

#[test]
fn upsert_is_monotonic_and_conflicts_require_full_resync() {
    let catalog = GlobalAccountCatalog::default();
    catalog
        .replace(vec![snapshot("C1", NOW - 2, "identity-a", 0.2, NOW + 100)])
        .unwrap();

    assert_eq!(
        catalog
            .upsert(snapshot("C1", NOW - 3, "identity-a", 0.9, NOW + 100))
            .unwrap(),
        ApplyOutcome::Unchanged { generation: 1 }
    );
    assert_eq!(
        catalog
            .upsert(snapshot("C1", NOW - 2, "identity-a", 0.3, NOW + 100))
            .unwrap(),
        ApplyOutcome::ResyncRequired { generation: 1 }
    );
    assert_eq!(
        catalog
            .upsert(snapshot("C1", NOW, "identity-b", 0.3, NOW + 100))
            .unwrap(),
        ApplyOutcome::ResyncRequired { generation: 1 }
    );

    let mut newer = snapshot("C1", NOW, "identity-a", 0.9, NOW + 200);
    newer.rate_limit.as_mut().unwrap().meters[0].utilization_observed_at = NOW - 3;
    assert_eq!(
        catalog.upsert(newer).unwrap(),
        ApplyOutcome::Applied { generation: 2 }
    );
    assert_eq!(catalog.projection(NOW)[0].meters[0].remaining_percent, 80);
}

#[test]
fn catalog_rejects_invalid_bounds() {
    let catalog = GlobalAccountCatalog::default();
    let mut too_many_meters = snapshot("C1", NOW, "one", 0.2, NOW + 100);
    too_many_meters.rate_limit.as_mut().unwrap().meters = (0..=MAX_METERS)
        .map(|index| meter(&format!("m{index}"), 0.1, NOW + 100, NOW))
        .collect();
    assert!(matches!(
        catalog.replace(vec![too_many_meters]),
        Err(CatalogError::InvalidPayload)
    ));

    let too_many_accounts = (1..=MAX_ACCOUNTS + 1)
        .map(|number| snapshot(&format!("C{number}"), NOW, "identity", 0.1, NOW + 100))
        .collect();
    assert!(matches!(
        catalog.replace(too_many_accounts),
        Err(CatalogError::InvalidPayload)
    ));
}

#[test]
fn projection_is_bounded_fresh_and_redacted() {
    let catalog = GlobalAccountCatalog::default();
    catalog
        .replace(vec![snapshot("C1", NOW, "owner-private", 0.25, NOW + 100)])
        .unwrap();
    let projection = catalog.projection(NOW);
    assert_eq!(projection[0].health, CatalogProjectionHealth::Healthy);
    assert_eq!(
        catalog.projection(NOW + FRESHNESS_SECONDS + 1)[0].health,
        CatalogProjectionHealth::Degraded
    );
    let json = serde_json::to_string(&projection).unwrap();
    assert!(!json.contains("owner-private"));
    assert!(!json.contains("sourceRef"));
    assert_eq!(projection[0].meters[0].remaining_percent, 75);
}
