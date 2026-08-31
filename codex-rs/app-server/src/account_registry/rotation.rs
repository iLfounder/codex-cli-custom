use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_app_server_protocol::RateLimitSnapshot;
use codex_app_server_protocol::ThreadAccountRotationMode;
use codex_core::TurnExecutionAccountDecision;
use codex_core::TurnExecutionAccountSelection;
use codex_core::TurnExecutionAccountSelector;
use codex_core::TurnExecutionAccountSelectorFuture;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_thread_store::ThreadAccountRotationMode as StoreRotationMode;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;

use super::AccountRegistry;
use super::ManifestSlotStatus;
use super::quota::QuotaCacheLookup;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RotationQuota {
    Fresh(Box<RateLimitSnapshot>, HashMap<String, RateLimitSnapshot>),
    MissingOrStale,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RotationCandidate {
    pub(crate) account_slot_id: String,
    pub(crate) account_number: u32,
    pub(crate) ready: bool,
    pub(crate) quota: RotationQuota,
    /// A caller-supplied hint is valid only after the caller has matched it to
    /// this candidate's current attempt generation and runtime version.
    pub(crate) hard_exhausted_hint: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RotationSelection {
    Selected(String),
    Unavailable,
}

pub(crate) struct RotationSelectionRequest<'a> {
    pub(crate) mode: ThreadAccountRotationMode,
    pub(crate) fixed_account_slot_id: Option<&'a str>,
    pub(crate) automatic_account_slot_ids: &'a [String],
    pub(crate) current_account_slot_id: Option<&'a str>,
    pub(crate) last_committed_account_slot_id: Option<&'a str>,
    pub(crate) now: i64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ExhaustionHintKey {
    pub(crate) thread_id: ThreadId,
    pub(crate) account_slot_id: String,
    pub(crate) execution_generation: u64,
}

pub(crate) struct AccountRotationService {
    registry: Arc<AccountRegistry>,
    thread_store: Arc<dyn ThreadStore>,
}

impl AccountRotationService {
    pub(crate) fn new(registry: Arc<AccountRegistry>, thread_store: Arc<dyn ThreadStore>) -> Self {
        Self {
            registry,
            thread_store,
        }
    }
}

impl TurnExecutionAccountSelector for AccountRotationService {
    fn select(
        &self,
        selection: TurnExecutionAccountSelection,
    ) -> TurnExecutionAccountSelectorFuture<'_> {
        Box::pin(async move {
            let policy = match self
                .thread_store
                .thread_account_rotation_policy(selection.thread_id)
                .await
            {
                Ok(policy) => policy,
                Err(ThreadStoreError::Unsupported { .. }) => {
                    return Ok(TurnExecutionAccountDecision::Keep);
                }
                Err(error) => {
                    return Err(CodexErr::Fatal(format!(
                        "account rotation store failed: {error}"
                    )));
                }
            };
            let mode = api_mode(policy.mode);
            let candidates = self
                .registry
                .rotation_candidates(
                    selection.thread_id,
                    selection.current_binding.generation,
                    mode == ThreadAccountRotationMode::ExhaustThenNext,
                )
                .await
                .map_err(|error| CodexErr::Fatal(error.message))?;
            let selected = select_account(
                RotationSelectionRequest {
                    mode,
                    fixed_account_slot_id: policy.fixed_account_slot_id.as_deref(),
                    automatic_account_slot_ids: &policy.automatic_account_slot_ids,
                    current_account_slot_id: Some(&selection.current_binding.slot_id),
                    last_committed_account_slot_id: policy
                        .last_committed_account_slot_id
                        .as_deref(),
                    now: chrono::Utc::now().timestamp(),
                },
                &candidates,
            );
            let RotationSelection::Selected(target_slot_id) = selected else {
                return Err(CodexErr::InvalidRequest(
                    "no eligible account slot is available".to_string(),
                ));
            };
            Ok(selection_decision(
                mode,
                target_slot_id,
                &selection.current_binding.slot_id,
                policy.revision,
            ))
        })
    }
}

fn selection_decision(
    mode: ThreadAccountRotationMode,
    target_slot_id: String,
    current_slot_id: &str,
    policy_revision: u64,
) -> TurnExecutionAccountDecision {
    if mode == ThreadAccountRotationMode::Fixed && target_slot_id == current_slot_id {
        TurnExecutionAccountDecision::Keep
    } else {
        TurnExecutionAccountDecision::Select {
            target_slot_id,
            policy_revision,
        }
    }
}

impl AccountRegistry {
    async fn rotation_candidates(
        self: &Arc<Self>,
        thread_id: ThreadId,
        execution_generation: u64,
        use_exhaustion_hints: bool,
    ) -> Result<Vec<RotationCandidate>, codex_app_server_protocol::JSONRPCErrorError> {
        self.reconcile().await?;
        let slots = self
            .state
            .read()
            .map_err(|_| crate::error_code::internal_error("account slot registry is unavailable"))?
            .slots
            .clone();
        let mut candidates = Vec::with_capacity(slots.len());
        for slot in slots {
            let runtime = self.runtime(&slot).await;
            let key = super::quota::QuotaCacheKey {
                account_slot_id: slot.manifest.account_slot_id.clone(),
                attempt_generation: slot.manifest.attempt_generation,
                runtime_version: runtime
                    .runtime_version
                    .load(std::sync::atomic::Ordering::Acquire),
            };
            let quota = match self.quota_cache.lookup(&key).await {
                QuotaCacheLookup::Fresh(snapshot) => RotationQuota::Fresh(
                    Box::new(snapshot.rate_limits),
                    snapshot.rate_limits_by_limit_id,
                ),
                QuotaCacheLookup::Unsupported => RotationQuota::MissingOrStale,
                QuotaCacheLookup::MissingOrStale => {
                    self.spawn_quota_refresh(key, Arc::clone(&runtime.auth_manager));
                    RotationQuota::MissingOrStale
                }
            };
            let hint = ExhaustionHintKey {
                thread_id,
                account_slot_id: slot.manifest.account_slot_id.clone(),
                execution_generation,
            };
            let hard_exhausted_hint = if use_exhaustion_hints {
                self.exhaustion_hints.lock().await.remove(&hint)
            } else {
                false
            };
            candidates.push(RotationCandidate {
                account_slot_id: slot.manifest.account_slot_id,
                account_number: slot.account_number,
                ready: (slot.manifest.is_default
                    || slot.manifest.status == ManifestSlotStatus::Ready)
                    && runtime.auth_manager.auth_cached().is_some(),
                quota,
                hard_exhausted_hint,
            });
        }
        Ok(candidates)
    }

    pub(crate) async fn record_exhaustion_hint(&self, hint: ExhaustionHintKey) {
        let mut hints = self.exhaustion_hints.lock().await;
        hints.retain(|existing| {
            existing.thread_id != hint.thread_id || existing.account_slot_id != hint.account_slot_id
        });
        if hints.len() >= 1_024 {
            hints.clear();
        }
        hints.insert(hint);
    }
}

fn api_mode(mode: StoreRotationMode) -> ThreadAccountRotationMode {
    match mode {
        StoreRotationMode::Fixed => ThreadAccountRotationMode::Fixed,
        StoreRotationMode::QuotaAware => ThreadAccountRotationMode::QuotaAware,
        StoreRotationMode::RoundRobin => ThreadAccountRotationMode::RoundRobin,
        StoreRotationMode::ExhaustThenNext => ThreadAccountRotationMode::ExhaustThenNext,
    }
}

pub(crate) fn select_account(
    request: RotationSelectionRequest<'_>,
    candidates: &[RotationCandidate],
) -> RotationSelection {
    if request.mode == ThreadAccountRotationMode::Fixed {
        return request
            .fixed_account_slot_id
            .and_then(|slot_id| {
                candidates
                    .iter()
                    .find(|candidate| candidate.account_slot_id == slot_id && candidate.ready)
            })
            .map(|candidate| RotationSelection::Selected(candidate.account_slot_id.clone()))
            .unwrap_or(RotationSelection::Unavailable);
    }

    let membership: HashSet<&str> = request
        .automatic_account_slot_ids
        .iter()
        .map(String::as_str)
        .collect();
    let mut eligible = candidates
        .iter()
        .filter(|candidate| {
            candidate.ready && membership.contains(candidate.account_slot_id.as_str())
        })
        .collect::<Vec<_>>();
    eligible.sort_by_key(|candidate| candidate.account_number);
    if eligible.is_empty() {
        return RotationSelection::Unavailable;
    }

    match request.mode {
        ThreadAccountRotationMode::Fixed => unreachable!(),
        ThreadAccountRotationMode::QuotaAware => select_quota_aware(&eligible, request.now),
        ThreadAccountRotationMode::RoundRobin => {
            let anchor = request
                .last_committed_account_slot_id
                .or(request.current_account_slot_id);
            let index = anchor
                .and_then(|slot_id| {
                    eligible
                        .iter()
                        .position(|candidate| candidate.account_slot_id == slot_id)
                })
                .map_or(0, |index| (index + 1) % eligible.len());
            RotationSelection::Selected(eligible[index].account_slot_id.clone())
        }
        ThreadAccountRotationMode::ExhaustThenNext => {
            let anchor = request
                .last_committed_account_slot_id
                .or(request.current_account_slot_id);
            if let Some(candidate) = anchor.and_then(|slot_id| {
                eligible
                    .iter()
                    .find(|candidate| candidate.account_slot_id == slot_id)
            }) && !candidate_hard_exhausted(candidate)
            {
                return RotationSelection::Selected(candidate.account_slot_id.clone());
            }
            let start = anchor
                .and_then(|slot_id| {
                    eligible
                        .iter()
                        .position(|candidate| candidate.account_slot_id == slot_id)
                })
                .map_or(0, |index| (index + 1) % eligible.len());
            eligible
                .iter()
                .cycle()
                .skip(start)
                .take(eligible.len())
                .find(|candidate| !candidate_hard_exhausted(candidate))
                .map(|candidate| RotationSelection::Selected(candidate.account_slot_id.clone()))
                .unwrap_or(RotationSelection::Unavailable)
        }
    }
}

fn select_quota_aware(candidates: &[&RotationCandidate], now: i64) -> RotationSelection {
    let mut best: Option<(&RotationCandidate, (i64, i64))> = None;
    let mut fallback = None;
    for candidate in candidates {
        if hard_exhausted(&candidate.quota) {
            continue;
        }
        let Some(score) = quota_score(&candidate.quota, now) else {
            fallback.get_or_insert(*candidate);
            continue;
        };
        let replace = best.as_ref().is_none_or(|(current, current_score)| {
            score.0 as i128 * current_score.1 as i128 > current_score.0 as i128 * score.1 as i128
                || (score.0 as i128 * current_score.1 as i128
                    == current_score.0 as i128 * score.1 as i128
                    && candidate.account_number < current.account_number)
        });
        if replace {
            best = Some((candidate, score));
        }
    }
    best.map(|(candidate, _)| candidate)
        .or(fallback)
        .map(|candidate| RotationSelection::Selected(candidate.account_slot_id.clone()))
        .unwrap_or(RotationSelection::Unavailable)
}

fn hard_exhausted(quota: &RotationQuota) -> bool {
    let RotationQuota::Fresh(rate_limits, by_limit_id) = quota else {
        return false;
    };
    std::iter::once(rate_limits.as_ref())
        .chain(by_limit_id.values())
        .any(|snapshot| {
            snapshot.spend_control_reached == Some(true)
                || snapshot.rate_limit_reached_type.is_some()
        })
}

fn candidate_hard_exhausted(candidate: &RotationCandidate) -> bool {
    candidate.hard_exhausted_hint || hard_exhausted(&candidate.quota)
}

fn quota_score(quota: &RotationQuota, now: i64) -> Option<(i64, i64)> {
    let RotationQuota::Fresh(rate_limits, by_limit_id) = quota else {
        return None;
    };
    let snapshots = if by_limit_id.is_empty() {
        vec![rate_limits.as_ref()]
    } else {
        by_limit_id.values().collect()
    };
    snapshots
        .into_iter()
        .flat_map(|snapshot| [snapshot.primary.as_ref(), snapshot.secondary.as_ref()])
        .flatten()
        .filter_map(|window| {
            let resets_at = window.resets_at?;
            (resets_at > now).then_some((
                i64::from((100 - window.used_percent).clamp(0, 100)),
                resets_at - now,
            ))
        })
        .min_by(|left, right| {
            (left.0 as i128 * right.1 as i128).cmp(&(right.0 as i128 * left.1 as i128))
        })
}

#[cfg(test)]
#[path = "rotation_tests.rs"]
mod tests;
