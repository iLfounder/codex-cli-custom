use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;

use super::identity::AccountId;
use super::runtime::CatalogAccount;
use super::runtime::CatalogState;
use super::runtime::FRESHNESS_SECONDS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RotationMode {
    Fixed,
    QuotaAware,
    RoundRobin,
    ExhaustThenNext,
}

pub(crate) struct CatalogSelectionRequest<'a> {
    pub(crate) mode: RotationMode,
    pub(crate) fixed_account_id: Option<AccountId>,
    pub(crate) automatic_account_ids: &'a [AccountId],
    pub(crate) current_account_id: Option<AccountId>,
    pub(crate) last_committed_account_id: Option<AccountId>,
    pub(crate) credential_readiness: &'a [CredentialReadiness],
    pub(crate) now: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CredentialReadiness {
    pub(crate) account_id: AccountId,
    pub(crate) ready: bool,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct CatalogSelectionToken {
    pub(crate) generation: u64,
    pub(crate) account_id: AccountId,
    source_ref: String,
}

impl fmt::Debug for CatalogSelectionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CatalogSelectionToken")
            .field("generation", &self.generation)
            .field("account_id", &self.account_id)
            .field("source_ref", &"<redacted>")
            .finish()
    }
}

impl CatalogSelectionToken {
    pub(crate) fn source_ref(&self) -> &str {
        &self.source_ref
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CatalogSelection {
    Selected(CatalogSelectionToken),
    Unavailable,
}

pub(super) fn select_catalog(
    state: &CatalogState,
    request: CatalogSelectionRequest<'_>,
) -> CatalogSelection {
    let readiness = request
        .credential_readiness
        .iter()
        .map(|entry| (entry.account_id, *entry))
        .collect::<HashMap<_, _>>();
    let selected = match request.mode {
        RotationMode::Fixed => request.fixed_account_id.filter(|account_id| {
            state
                .accounts
                .get(account_id)
                .is_some_and(|account| fixed_eligible(account, readiness.get(account_id)))
        }),
        RotationMode::QuotaAware => select_quota_aware(
            state,
            request.automatic_account_ids,
            &readiness,
            request.now,
        ),
        RotationMode::RoundRobin => select_round_robin(
            state,
            request.automatic_account_ids,
            &readiness,
            request
                .last_committed_account_id
                .or(request.current_account_id),
            request.now,
        ),
        RotationMode::ExhaustThenNext => select_exhaust_then_next(
            state,
            request.automatic_account_ids,
            &readiness,
            request
                .last_committed_account_id
                .or(request.current_account_id),
            request.now,
        ),
    };
    let Some(account_id) = selected else {
        return CatalogSelection::Unavailable;
    };
    let account = &state.accounts[&account_id];
    CatalogSelection::Selected(CatalogSelectionToken {
        generation: state.generation,
        account_id,
        source_ref: account.source_ref.clone().unwrap_or_default(),
    })
}

fn fixed_eligible(account: &CatalogAccount, readiness: Option<&CredentialReadiness>) -> bool {
    account.source_ref.is_some() && readiness.is_some_and(|entry| entry.ready)
}

fn automatic_ready(
    account: &CatalogAccount,
    readiness: Option<&CredentialReadiness>,
    now: i64,
) -> bool {
    fixed_eligible(account, readiness) && account.ok && account.fresh(now)
}

fn automatic_ready_candidates<'a>(
    state: &'a CatalogState,
    membership: &[AccountId],
    readiness: &HashMap<AccountId, CredentialReadiness>,
    now: i64,
) -> Vec<&'a CatalogAccount> {
    let membership = membership.iter().copied().collect::<HashSet<_>>();
    state
        .accounts
        .values()
        .filter(|account| {
            membership.contains(&account.id)
                && automatic_ready(account, readiness.get(&account.id), now)
        })
        .collect()
}

fn select_quota_aware(
    state: &CatalogState,
    membership: &[AccountId],
    readiness: &HashMap<AccountId, CredentialReadiness>,
    now: i64,
) -> Option<AccountId> {
    automatic_ready_candidates(state, membership, readiness, now)
        .into_iter()
        .filter(|account| !account.hard_exhausted(now))
        .filter_map(|account| bottleneck_score(account, now).map(|score| (account.id, score)))
        .max_by(|(left_id, left), (right_id, right)| {
            left.total_cmp(right).then_with(|| right_id.cmp(left_id))
        })
        .map(|(account_id, _)| account_id)
}

fn bottleneck_score(account: &CatalogAccount, now: i64) -> Option<f64> {
    if account.meters.is_empty() {
        return None;
    }
    account
        .meters
        .iter()
        .map(|meter| {
            let reset_at = meter.reset_at?;
            (reset_at > now
                && meter.utilization_observed_at <= now
                && now - meter.utilization_observed_at <= FRESHNESS_SECONDS)
                .then_some((1.0 - meter.utilization) / (reset_at - now) as f64)
        })
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .min_by(f64::total_cmp)
}

fn select_round_robin(
    state: &CatalogState,
    membership: &[AccountId],
    readiness: &HashMap<AccountId, CredentialReadiness>,
    anchor: Option<AccountId>,
    now: i64,
) -> Option<AccountId> {
    let candidates = automatic_ready_candidates(state, membership, readiness, now);
    next_after_anchor(&candidates, anchor)
}

fn select_exhaust_then_next(
    state: &CatalogState,
    membership: &[AccountId],
    readiness: &HashMap<AccountId, CredentialReadiness>,
    anchor: Option<AccountId>,
    now: i64,
) -> Option<AccountId> {
    let candidates = automatic_ready_candidates(state, membership, readiness, now);
    if anchor.is_some_and(|anchor| {
        candidates
            .iter()
            .any(|candidate| candidate.id == anchor && !candidate.hard_exhausted(now))
    }) {
        return anchor;
    }
    let eligible = candidates
        .into_iter()
        .filter(|candidate| !candidate.hard_exhausted(now))
        .collect::<Vec<_>>();
    next_after_anchor(&eligible, anchor)
}

fn next_after_anchor(
    candidates: &[&CatalogAccount],
    anchor: Option<AccountId>,
) -> Option<AccountId> {
    candidates
        .iter()
        .find(|candidate| anchor.is_none_or(|anchor| candidate.id > anchor))
        .or_else(|| candidates.first())
        .map(|candidate| candidate.id)
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
