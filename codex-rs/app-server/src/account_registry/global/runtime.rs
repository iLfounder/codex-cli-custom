use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::RwLock;

use serde::Serialize;

use super::identity::AccountId;
use super::selection::CatalogSelection;
use super::selection::CatalogSelectionRequest;
use super::token_manager::CatalogError;
use super::token_manager::MAX_ACCOUNTS;
use super::token_manager::MAX_METERS;
use super::token_manager::RawMeter;
use super::token_manager::RawRateLimit;
use super::token_manager::RawSnapshot;
use super::token_manager::TokenManagerEvent;

const PROVIDER_TYPE: &str = "codex-chatgpt";
pub(super) const FRESHNESS_SECONDS: i64 = 180;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CatalogProjectionHealth {
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogQuotaMeterProjection {
    pub(crate) id: String,
    pub(crate) label: Option<String>,
    pub(crate) remaining_percent: u32,
    pub(crate) resets_at: Option<i64>,
    pub(crate) observed_at: i64,
    pub(crate) stale_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogAccountProjection {
    pub(crate) account_slot_id: String,
    pub(crate) account_number: u32,
    pub(crate) fetched_at: i64,
    pub(crate) health: CatalogProjectionHealth,
    pub(crate) meters: Vec<CatalogQuotaMeterProjection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApplyOutcome {
    Applied { generation: u64 },
    Unchanged { generation: u64 },
    ResyncRequired { generation: u64 },
}

#[derive(Clone, PartialEq)]
pub(super) struct CatalogAccount {
    pub(super) id: AccountId,
    pub(super) source_ref: Option<String>,
    fetched_at: i64,
    pub(super) ok: bool,
    pub(super) quota_status: QuotaStatus,
    pub(super) meters: Vec<CatalogMeter>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QuotaStatus {
    Allowed,
    Warning,
    Rejected,
    Unknown,
}

#[derive(Clone, PartialEq)]
pub(super) struct CatalogMeter {
    id: String,
    label: Option<String>,
    pub(super) utilization: f64,
    pub(super) reset_at: Option<i64>,
    observed_at: i64,
    pub(super) utilization_observed_at: i64,
    state: MeterState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MeterState {
    Normal,
    Warning,
    Exhausted,
    Unknown,
}

#[derive(Default)]
pub(super) struct CatalogState {
    initialized: bool,
    pub(super) generation: u64,
    pub(super) accounts: BTreeMap<AccountId, CatalogAccount>,
}

#[derive(Default)]
pub(crate) struct GlobalAccountCatalog {
    state: RwLock<CatalogState>,
}

impl GlobalAccountCatalog {
    pub(crate) fn generation(&self) -> u64 {
        self.state.read().map_or(0, |state| state.generation)
    }

    pub(crate) fn apply_event(
        &self,
        event: TokenManagerEvent,
    ) -> Result<ApplyOutcome, CatalogError> {
        match event {
            TokenManagerEvent::Initial(accounts) => self.replace(accounts),
            TokenManagerEvent::Snapshot(account) => self.upsert(account),
        }
    }

    pub(crate) fn replace(
        &self,
        snapshots: Vec<RawSnapshot>,
    ) -> Result<ApplyOutcome, CatalogError> {
        if snapshots.len() > MAX_ACCOUNTS {
            return Err(CatalogError::InvalidPayload);
        }
        let mut accounts = BTreeMap::new();
        let mut source_refs = HashSet::new();
        for snapshot in snapshots {
            let Some(account) = normalize(snapshot)? else {
                continue;
            };
            if account
                .source_ref
                .as_ref()
                .is_some_and(|source_ref| !source_refs.insert(source_ref.clone()))
            {
                return Err(CatalogError::InvalidPayload);
            }
            if accounts.insert(account.id, account).is_some() {
                return Err(CatalogError::InvalidPayload);
            }
        }
        let mut state = self
            .state
            .write()
            .map_err(|_| CatalogError::InvalidPayload)?;
        if state.initialized && state.accounts == accounts {
            return Ok(ApplyOutcome::Unchanged {
                generation: state.generation,
            });
        }
        state.initialized = true;
        state.generation = state.generation.saturating_add(1);
        state.accounts = accounts;
        Ok(ApplyOutcome::Applied {
            generation: state.generation,
        })
    }

    pub(crate) fn upsert(&self, snapshot: RawSnapshot) -> Result<ApplyOutcome, CatalogError> {
        let Some(mut incoming) = normalize(snapshot)? else {
            return Ok(ApplyOutcome::Unchanged {
                generation: self.generation(),
            });
        };
        let mut state = self
            .state
            .write()
            .map_err(|_| CatalogError::InvalidPayload)?;
        if !state.initialized {
            return Ok(ApplyOutcome::ResyncRequired {
                generation: state.generation,
            });
        }
        if incoming.source_ref.as_ref().is_some_and(|source_ref| {
            state.accounts.values().any(|account| {
                account.id != incoming.id && account.source_ref.as_ref() == Some(source_ref)
            })
        }) {
            return Ok(ApplyOutcome::ResyncRequired {
                generation: state.generation,
            });
        }
        let Some(current) = state.accounts.get(&incoming.id) else {
            if state.accounts.len() >= MAX_ACCOUNTS {
                return Ok(ApplyOutcome::ResyncRequired {
                    generation: state.generation,
                });
            }
            state.generation = state.generation.saturating_add(1);
            let generation = state.generation;
            state.accounts.insert(incoming.id, incoming);
            return Ok(ApplyOutcome::Applied { generation });
        };
        if current.source_ref != incoming.source_ref {
            return Ok(ApplyOutcome::ResyncRequired {
                generation: state.generation,
            });
        }
        match incoming.fetched_at.cmp(&current.fetched_at) {
            Ordering::Less => {
                return Ok(ApplyOutcome::Unchanged {
                    generation: state.generation,
                });
            }
            Ordering::Equal if &incoming == current => {
                return Ok(ApplyOutcome::Unchanged {
                    generation: state.generation,
                });
            }
            Ordering::Equal => {
                return Ok(ApplyOutcome::ResyncRequired {
                    generation: state.generation,
                });
            }
            Ordering::Greater => {}
        }
        if !merge_monotonic_meters(&mut incoming, current) {
            return Ok(ApplyOutcome::ResyncRequired {
                generation: state.generation,
            });
        }
        if &incoming == current {
            return Ok(ApplyOutcome::Unchanged {
                generation: state.generation,
            });
        }
        state.generation = state.generation.saturating_add(1);
        let generation = state.generation;
        state.accounts.insert(incoming.id, incoming);
        Ok(ApplyOutcome::Applied { generation })
    }

    pub(crate) fn projection(&self, now: i64) -> Vec<CatalogAccountProjection> {
        self.state.read().map_or_else(
            |_| Vec::new(),
            |state| {
                state
                    .accounts
                    .values()
                    .map(|account| account.projection(now))
                    .collect()
            },
        )
    }

    pub(crate) fn select(&self, request: CatalogSelectionRequest<'_>) -> CatalogSelection {
        let Ok(state) = self.state.read() else {
            return CatalogSelection::Unavailable;
        };
        super::selection::select_catalog(&state, request)
    }
}

impl CatalogAccount {
    pub(super) fn fresh(&self, now: i64) -> bool {
        self.fetched_at <= now && now - self.fetched_at <= FRESHNESS_SECONDS
    }

    fn projection(&self, now: i64) -> CatalogAccountProjection {
        CatalogAccountProjection {
            account_slot_id: self.id.to_string(),
            account_number: self.id.number(),
            fetched_at: self.fetched_at,
            health: if self.source_ref.is_none() {
                CatalogProjectionHealth::Unavailable
            } else if self.ok && self.fresh(now) {
                CatalogProjectionHealth::Healthy
            } else {
                CatalogProjectionHealth::Degraded
            },
            meters: self
                .meters
                .iter()
                .map(|meter| CatalogQuotaMeterProjection {
                    id: meter.id.clone(),
                    label: meter.label.clone(),
                    remaining_percent: ((1.0 - meter.utilization) * 100.0).round() as u32,
                    resets_at: meter.reset_at,
                    observed_at: meter.utilization_observed_at,
                    stale_at: meter
                        .utilization_observed_at
                        .saturating_add(FRESHNESS_SECONDS),
                })
                .collect(),
        }
    }

    pub(super) fn hard_exhausted(&self, now: i64) -> bool {
        self.quota_status == QuotaStatus::Rejected
            || self.meters.iter().any(|meter| {
                let reset_is_current = meter.reset_at.is_none_or(|reset_at| reset_at > now);
                (meter.state == MeterState::Exhausted
                    && meter.observed_at <= now
                    && now.saturating_sub(meter.observed_at) <= FRESHNESS_SECONDS
                    && reset_is_current)
                    || (meter.utilization >= 1.0
                        && meter.utilization_observed_at <= now
                        && now.saturating_sub(meter.utilization_observed_at) <= FRESHNESS_SECONDS
                        && reset_is_current)
            })
    }
}

fn normalize(snapshot: RawSnapshot) -> Result<Option<CatalogAccount>, CatalogError> {
    if snapshot.provider_type != PROVIDER_TYPE {
        return Ok(None);
    }
    let Some(id) = AccountId::parse(&snapshot.label) else {
        return Ok(None);
    };
    if snapshot.fetched_at <= 0 {
        return Err(CatalogError::InvalidPayload);
    }
    let RawRateLimit { meters, status } = snapshot.rate_limit.unwrap_or(RawRateLimit {
        meters: Vec::new(),
        status: String::new(),
    });
    if meters.len() > MAX_METERS {
        return Err(CatalogError::InvalidPayload);
    }
    let mut meter_ids = HashSet::new();
    let meters = meters
        .into_iter()
        .map(|meter| normalize_meter(meter, &mut meter_ids))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(CatalogAccount {
        id,
        source_ref: snapshot.source_ref.filter(|value| !value.is_empty()),
        fetched_at: snapshot.fetched_at,
        ok: snapshot.ok,
        quota_status: match status.as_str() {
            "allowed" => QuotaStatus::Allowed,
            "allowed_warning" => QuotaStatus::Warning,
            "rejected" => QuotaStatus::Rejected,
            "" | "unknown" => QuotaStatus::Unknown,
            _ => return Err(CatalogError::InvalidPayload),
        },
        meters,
    }))
}

fn normalize_meter(
    meter: RawMeter,
    meter_ids: &mut HashSet<String>,
) -> Result<CatalogMeter, CatalogError> {
    if meter.id.is_empty()
        || !meter_ids.insert(meter.id.clone())
        || !meter.utilization.is_finite()
        || !(0.0..=1.0).contains(&meter.utilization)
    {
        return Err(CatalogError::InvalidPayload);
    }
    Ok(CatalogMeter {
        id: meter.id,
        label: (!meter.label.is_empty()).then_some(meter.label),
        utilization: meter.utilization,
        reset_at: (meter.reset_at > 0).then_some(meter.reset_at),
        observed_at: meter.observed_at,
        utilization_observed_at: meter.utilization_observed_at,
        state: match meter.state.as_str() {
            "normal" => MeterState::Normal,
            "warning" => MeterState::Warning,
            "exhausted" => MeterState::Exhausted,
            "" | "unknown" => MeterState::Unknown,
            _ => return Err(CatalogError::InvalidPayload),
        },
    })
}

fn merge_monotonic_meters(incoming: &mut CatalogAccount, current: &CatalogAccount) -> bool {
    let current_meters = current
        .meters
        .iter()
        .map(|meter| (meter.id.as_str(), meter))
        .collect::<HashMap<_, _>>();
    for meter in &mut incoming.meters {
        let Some(previous) = current_meters.get(meter.id.as_str()) else {
            continue;
        };
        match meter
            .utilization_observed_at
            .cmp(&previous.utilization_observed_at)
        {
            Ordering::Less => {
                meter.utilization = previous.utilization;
                meter.utilization_observed_at = previous.utilization_observed_at;
            }
            Ordering::Equal if meter.utilization != previous.utilization => return false,
            Ordering::Equal | Ordering::Greater => {}
        }
        match meter.observed_at.cmp(&previous.observed_at) {
            Ordering::Less => {
                meter.reset_at = previous.reset_at;
                meter.observed_at = previous.observed_at;
                meter.state = previous.state;
            }
            Ordering::Equal
                if meter.reset_at != previous.reset_at || meter.state != previous.state =>
            {
                return false;
            }
            Ordering::Equal | Ordering::Greater => {}
        }
    }
    true
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
