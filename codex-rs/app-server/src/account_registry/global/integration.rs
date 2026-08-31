use std::collections::HashMap;
use std::sync::Arc;

use codex_app_server_protocol::AccountSlotHealth;
use codex_app_server_protocol::AccountSlotQuotaMeter;
use codex_app_server_protocol::AccountSlotQuotaSnapshot;
use codex_app_server_protocol::AccountSlotRateLimitsReadResponse;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RateLimitSnapshot;
use codex_app_server_protocol::RateLimitWindow;
use codex_core::ExecutionAccountContext;
use codex_login::AuthManager;
use codex_login::CredentialRevision;
use codex_model_provider::create_model_provider;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::ExecutionAccountBinding;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;

use super as global;
use super::directory::subscription_source_ref;
use super::token_manager::TokenManagerEvent;
use crate::account_registry::AccountRegistry;
use crate::auth_mode::auth_mode_to_api;
use crate::error_code::invalid_request;

pub(crate) struct GlobalAccountRuntime {
    pub(crate) credential_revision: Option<CredentialRevision>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) source_ref: String,
    pub(crate) binding_transition: Arc<Mutex<()>>,
}

impl AccountRegistry {
    pub(crate) fn spawn_global_catalog(
        self: &Arc<Self>,
        outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    ) {
        let Some(client) = self.token_manager_client.clone() else {
            return;
        };
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if registry
                    .refresh_global_directory()
                    .process_account_id
                    .is_none()
                {
                    tokio::time::sleep(global::FULL_REFRESH_INTERVAL).await;
                    continue;
                }
                match registry.refresh_global_catalog(&client).await {
                    Ok(outcome) => match outcome {
                        global::ApplyOutcome::Applied { generation } => {
                            outgoing
                                .send_server_notification(
                                    codex_app_server_protocol::ServerNotification::AccountSlotInventoryChanged(
                                        codex_app_server_protocol::AccountSlotInventoryChangedNotification {
                                            registry_revision: generation,
                                        },
                                    ),
                                )
                                .await;
                        }
                        global::ApplyOutcome::Unchanged { .. } => {}
                        global::ApplyOutcome::ResyncRequired { .. } => unreachable!(
                            "full TokenManager replacement cannot require another resync"
                        ),
                    },
                    Err(error) => {
                        tracing::warn!("TokenManager account snapshot unavailable: {error}")
                    }
                }

                let Ok(mut events) = client.subscribe().await else {
                    tokio::time::sleep(global::FULL_REFRESH_INTERVAL).await;
                    continue;
                };
                let refresh = tokio::time::sleep(global::FULL_REFRESH_INTERVAL);
                tokio::pin!(refresh);
                let refresh_elapsed = loop {
                    tokio::select! {
                        event = events.recv() => {
                            let Some(Ok(event)) = event else {
                                break false;
                            };
                            let is_full_replacement =
                                matches!(&event, TokenManagerEvent::Initial(_));
                            let changed_account_id = event.snapshot_account_id();
                            match registry.global_catalog.apply_event(event) {
                                Ok(global::ApplyOutcome::Applied { generation }) => {
                                    if is_full_replacement {
                                        outgoing.send_server_notification(
                                            codex_app_server_protocol::ServerNotification::AccountSlotInventoryChanged(
                                                codex_app_server_protocol::AccountSlotInventoryChangedNotification {
                                                    registry_revision: generation,
                                                },
                                            ),
                                        ).await;
                                    } else if let Some(account_id) = changed_account_id {
                                        let now = chrono::Utc::now().timestamp();
                                        let Some(projection) = registry
                                            .global_catalog
                                            .projection(now)
                                            .into_iter()
                                            .find(|projection| {
                                                projection.account_slot_id == account_id.to_string()
                                            })
                                        else {
                                            break false;
                                        };
                                        let slot = registry
                                            .global_snapshot(projection, generation, now)
                                            .await;
                                        outgoing.send_server_notification(
                                            codex_app_server_protocol::ServerNotification::AccountSlotChanged(
                                                codex_app_server_protocol::AccountSlotChangedNotification {
                                                    registry_revision: generation,
                                                    slot,
                                                },
                                            ),
                                        ).await;
                                    }
                                }
                                Ok(global::ApplyOutcome::Unchanged { .. }) => {}
                                Ok(global::ApplyOutcome::ResyncRequired { .. }) | Err(_) => break false,
                            }
                        }
                        () = &mut refresh => break true,
                    }
                };
                wait_before_catalog_reconnect(refresh_elapsed).await;
            }
        });
    }

    async fn refresh_global_catalog(
        &self,
        client: &global::TokenManagerClient,
    ) -> Result<global::ApplyOutcome, global::CatalogError> {
        let _refresh = self
            .global_catalog_refresh
            .acquire()
            .await
            .map_err(|_| global::CatalogError::Request)?;
        let snapshots = client.fetch_full().await?;
        self.global_catalog.replace(snapshots)
    }

    pub(crate) async fn ensure_global_catalog(&self) -> Result<u64, CodexErr> {
        let generation = self.global_catalog.generation();
        if generation > 0 {
            return Ok(generation);
        }
        let Some(client) = self.token_manager_client.as_ref() else {
            return Err(CodexErr::InvalidRequest(
                "global account catalog is unavailable".to_string(),
            ));
        };
        let _refresh = self.global_catalog_refresh.acquire().await.map_err(|_| {
            CodexErr::InvalidRequest("global account catalog is unavailable".to_string())
        })?;
        let generation = self.global_catalog.generation();
        if generation > 0 {
            return Ok(generation);
        }
        let snapshots = client.fetch_full().await.map_err(|_| {
            CodexErr::InvalidRequest("global account catalog is unavailable".to_string())
        })?;
        self.global_catalog.replace(snapshots).map_err(|_| {
            CodexErr::InvalidRequest("global account catalog is unavailable".to_string())
        })?;
        let generation = self.global_catalog.generation();
        (generation > 0).then_some(generation).ok_or_else(|| {
            CodexErr::InvalidRequest("global account catalog is unavailable".to_string())
        })
    }

    pub(crate) async fn global_runtime(
        &self,
        account_id: global::AccountId,
    ) -> Result<Arc<GlobalAccountRuntime>, CodexErr> {
        let directory = self.refresh_global_directory();
        let auth_home = directory.homes.get(&account_id).cloned().ok_or_else(|| {
            CodexErr::InvalidRequest(format!("execution account `{account_id}` is unavailable"))
        })?;
        let mut auth_config = self.auth_config_template.clone();
        auth_config.codex_home = auth_home.clone();
        auth_config.auth_credentials_store_mode =
            codex_config::types::AuthCredentialsStoreMode::File;
        let auth_manager = AuthManager::shared_from_read_only_auth_config(auth_config)
            .await
            .map_err(|_| {
                CodexErr::InvalidRequest(format!("execution account `{account_id}` is unavailable"))
            })?;
        let auth = auth_manager.auth_cached().ok_or_else(|| {
            CodexErr::InvalidRequest(format!("execution account `{account_id}` is not logged in"))
        })?;
        let credential_revision = auth_manager.credential_revision();
        let account_identity = auth.get_account_id().ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "execution account `{account_id}` has no stable identity"
            ))
        })?;
        let source_ref =
            subscription_source_ref(&account_identity, &auth_home).ok_or_else(|| {
                CodexErr::InvalidRequest(format!(
                    "execution account `{account_id}` has invalid identity"
                ))
            })?;

        let mut runtimes = self.global_runtimes.lock().await;
        if let Some(runtime) = runtimes.get(&account_id)
            && runtime.credential_revision == credential_revision
            && runtime.source_ref == source_ref
        {
            return Ok(Arc::clone(runtime));
        }
        let provider = create_model_provider(
            self.config.model_provider.clone(),
            Some(Arc::clone(&auth_manager)),
        );
        let models_manager = provider.models_manager(auth_home, self.config.model_catalog.clone());
        let binding_transition = runtimes.get(&account_id).map_or_else(
            || Arc::new(Mutex::new(())),
            |runtime| Arc::clone(&runtime.binding_transition),
        );
        let runtime = Arc::new(GlobalAccountRuntime {
            credential_revision,
            auth_manager,
            models_manager,
            source_ref,
            binding_transition,
        });
        runtimes.insert(account_id, Arc::clone(&runtime));
        Ok(runtime)
    }

    pub(crate) async fn global_snapshot(
        &self,
        projection: global::CatalogAccountProjection,
        generation: u64,
        now: i64,
    ) -> AccountSlotSnapshot {
        let account_id = global::AccountId::parse(&projection.account_slot_id);
        let runtime = match account_id {
            Some(account_id) => self.global_runtime(account_id).await.ok(),
            None => None,
        };
        let ready = match (account_id, runtime.as_ref()) {
            (Some(account_id), Some(runtime)) => matches!(
                self.global_catalog.select(global::CatalogSelectionRequest {
                    mode: global::RotationMode::Fixed,
                    fixed_account_id: Some(account_id),
                    automatic_account_ids: &[],
                    current_account_id: None,
                    last_committed_account_id: None,
                    credential_readiness: &[global::CredentialReadiness {
                        account_id,
                        ready: true,
                    }],
                    now,
                }),
                global::CatalogSelection::Selected(token)
                    if token.source_ref() == runtime.source_ref
            ),
            _ => false,
        };
        let quota = (!projection.meters.is_empty()).then(|| {
            let observed_at = projection
                .meters
                .iter()
                .map(|meter| meter.observed_at)
                .min()
                .unwrap_or(projection.fetched_at);
            let stale_at = projection
                .meters
                .iter()
                .map(|meter| meter.stale_at)
                .min()
                .unwrap_or(projection.fetched_at);
            AccountSlotQuotaSnapshot {
                meters: projection
                    .meters
                    .into_iter()
                    .map(|meter| AccountSlotQuotaMeter {
                        id: meter.id,
                        label: meter.label,
                        remaining_percent: meter.remaining_percent,
                        resets_at: meter.resets_at,
                    })
                    .collect(),
                observed_at,
                stale_at,
            }
        });
        let process_account_id = self.refresh_global_directory().process_account_id;
        let local_default = if account_id == process_account_id {
            let (slot, manifest_error, revision) = self
                .state
                .read()
                .ok()
                .and_then(|state| {
                    state
                        .slots
                        .iter()
                        .find(|slot| slot.manifest.is_default)
                        .cloned()
                        .map(|slot| (slot, state.manifest_error, state.revision))
                })
                .map_or((None, None, 0), |(slot, error, revision)| {
                    (Some(slot), error, revision)
                });
            match slot {
                Some(slot) => {
                    let capability = self.capability(manifest_error);
                    let config = self.load_latest_config().await;
                    Some(self.snapshot(&slot, revision, &capability, &config).await)
                }
                None => None,
            }
        } else {
            None
        };
        AccountSlotSnapshot {
            account_slot_id: projection.account_slot_id.clone(),
            account_number: projection.account_number,
            label: projection.account_slot_id,
            is_default: account_id == process_account_id,
            status: if ready {
                AccountSlotStatus::Ready
            } else {
                AccountSlotStatus::LoginRequired
            },
            health: match projection.health {
                global::CatalogProjectionHealth::Healthy => AccountSlotHealth::Healthy,
                global::CatalogProjectionHealth::Degraded => AccountSlotHealth::Degraded,
                global::CatalogProjectionHealth::Unavailable => AccountSlotHealth::Unavailable,
            },
            quota,
            auth_mode: runtime
                .as_ref()
                .and_then(|runtime| runtime.auth_manager.auth_mode())
                .map(auth_mode_to_api),
            attempt_generation: local_default
                .as_ref()
                .map_or(0, |slot| slot.attempt_generation),
            registry_revision: generation,
            active_login_operation_id: local_default
                .as_ref()
                .and_then(|slot| slot.active_login_operation_id.clone()),
            error_code: local_default
                .as_ref()
                .and_then(|slot| slot.error_code.clone()),
            actions: local_default.map_or_else(Vec::new, |slot| slot.actions),
            updated_at: projection.fetched_at,
        }
    }

    pub(crate) fn global_rate_limits(
        &self,
        account_slot_id: &str,
    ) -> Option<Result<AccountSlotRateLimitsReadResponse, JSONRPCErrorError>> {
        global::AccountId::parse(account_slot_id)?;
        let projection = self
            .global_catalog
            .projection(chrono::Utc::now().timestamp())
            .into_iter()
            .find(|account| account.account_slot_id == account_slot_id);
        Some(
            projection
                .ok_or_else(|| invalid_request("account slot is unavailable"))
                .and_then(|projection| {
                    if projection.meters.is_empty() {
                        return Err(invalid_request("account slot rate limits are unavailable"));
                    }
                    let captured_at = projection
                        .meters
                        .iter()
                        .map(|meter| meter.observed_at)
                        .min()
                        .unwrap_or(projection.fetched_at);
                    let stale_at = projection
                        .meters
                        .iter()
                        .map(|meter| meter.stale_at)
                        .min()
                        .unwrap_or(projection.fetched_at);
                    let rate_limits_by_limit_id = projection
                        .meters
                        .into_iter()
                        .map(|meter| {
                            let limit_id = meter.id.clone();
                            (
                                limit_id.clone(),
                                RateLimitSnapshot {
                                    limit_id: Some(limit_id),
                                    limit_name: meter.label,
                                    primary: Some(RateLimitWindow {
                                        used_percent: (100_u32
                                            .saturating_sub(meter.remaining_percent))
                                            as i32,
                                        window_duration_mins: None,
                                        resets_at: meter.resets_at,
                                    }),
                                    secondary: None,
                                    credits: None,
                                    individual_limit: None,
                                    spend_control_reached: None,
                                    plan_type: None,
                                    rate_limit_reached_type: None,
                                },
                            )
                        })
                        .collect::<HashMap<_, _>>();
                    let rate_limits = rate_limits_by_limit_id
                        .values()
                        .next()
                        .cloned()
                        .ok_or_else(|| {
                            invalid_request("account slot rate limits are unavailable")
                        })?;
                    Ok(AccountSlotRateLimitsReadResponse {
                        account_slot_id: projection.account_slot_id,
                        attempt_generation: 0,
                        captured_at,
                        stale_at,
                        rate_limits,
                        rate_limits_by_limit_id,
                    })
                }),
        )
    }

    pub(crate) async fn resolve_global_execution_account(
        &self,
        binding: ExecutionAccountBinding,
    ) -> Result<(Arc<ExecutionAccountContext>, OwnedMutexGuard<()>), CodexErr> {
        self.ensure_global_catalog().await?;
        let account_id = global::AccountId::parse(&binding.slot_id).ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "execution account slot `{}` is unavailable",
                binding.slot_id
            ))
        })?;
        let runtime = self.global_runtime(account_id).await?;
        let selected = self.global_catalog.select(global::CatalogSelectionRequest {
            mode: global::RotationMode::Fixed,
            fixed_account_id: Some(account_id),
            automatic_account_ids: &[],
            current_account_id: None,
            last_committed_account_id: None,
            credential_readiness: &[global::CredentialReadiness {
                account_id,
                ready: true,
            }],
            now: chrono::Utc::now().timestamp(),
        });
        let global::CatalogSelection::Selected(token) = selected else {
            return Err(CodexErr::InvalidRequest(format!(
                "execution account slot `{}` is unavailable",
                binding.slot_id
            )));
        };
        if token.source_ref() != runtime.source_ref {
            return Err(CodexErr::InvalidRequest(format!(
                "execution account slot `{}` identity changed",
                binding.slot_id
            )));
        }
        let binding_transition = Arc::clone(&runtime.binding_transition).lock_owned().await;
        Ok((
            Arc::new(ExecutionAccountContext {
                binding,
                auth_manager: Arc::clone(&runtime.auth_manager),
                models_manager: Arc::clone(&runtime.models_manager),
            }),
            binding_transition,
        ))
    }
}

async fn wait_before_catalog_reconnect(refresh_elapsed: bool) {
    if !refresh_elapsed {
        tokio::time::sleep(global::FULL_REFRESH_INTERVAL).await;
    }
}

#[cfg(test)]
#[path = "integration_tests.rs"]
mod tests;
