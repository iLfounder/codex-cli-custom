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
    pub(crate) async fn notify_global_inventory_if_changed(
        &self,
        outgoing: &crate::outgoing_message::OutgoingMessageSender,
    ) -> global::GlobalAccountDirectory {
        let directory = self.refresh_global_directory();
        let (_, update) = self
            .global_inventory_snapshot(&directory, chrono::Utc::now().timestamp())
            .await;
        if self
            .claim_global_inventory_notification(update.revision)
            .is_some()
        {
            send_inventory_changed(outgoing, update.revision).await;
        }
        directory
    }

    pub(crate) fn spawn_global_catalog(
        self: &Arc<Self>,
        outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    ) {
        let client = self.token_manager_client.clone();
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let directory = registry.notify_global_inventory_if_changed(&outgoing).await;
                if directory.process_account_id.is_none() {
                    tokio::time::sleep(global::FULL_REFRESH_INTERVAL).await;
                    continue;
                }
                let Some(client) = client.as_ref() else {
                    tokio::time::sleep(global::FULL_REFRESH_INTERVAL).await;
                    continue;
                };
                match registry.refresh_global_catalog(client).await {
                    Ok(outcome) => match outcome {
                        global::ApplyOutcome::Applied { .. } => {
                            let directory = registry.refresh_global_directory();
                            let (_, update) = registry
                                .global_inventory_snapshot(
                                    &directory,
                                    chrono::Utc::now().timestamp(),
                                )
                                .await;
                            if registry
                                .claim_global_inventory_notification(update.revision)
                                .is_some()
                            {
                                send_inventory_changed(&outgoing, update.revision).await;
                            }
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
                                Ok(global::ApplyOutcome::Applied { .. }) => {
                                    let now = chrono::Utc::now().timestamp();
                                    let directory = registry.refresh_global_directory();
                                    let (snapshots, update) = registry
                                        .global_inventory_snapshot(&directory, now)
                                        .await;
                                    let Some(directory_changed) = registry
                                        .claim_global_inventory_notification(update.revision)
                                    else {
                                        continue;
                                    };
                                    if is_full_replacement || directory_changed {
                                        send_inventory_changed(&outgoing, update.revision).await;
                                    } else if let Some(slot) = changed_account_id.and_then(|account_id| {
                                        snapshots.into_iter().find(|slot| {
                                            slot.account_slot_id == account_id.to_string()
                                        })
                                    }) {
                                        outgoing.send_server_notification(
                                            codex_app_server_protocol::ServerNotification::AccountSlotChanged(
                                                codex_app_server_protocol::AccountSlotChangedNotification {
                                                    registry_revision: update.revision,
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

    pub(super) async fn refresh_global_catalog(
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
        self.global_runtime_with_directory(account_id, &directory)
            .await
    }

    /// Resolve a global runtime using a caller-provided directory snapshot.
    ///
    /// Selection probes several accounts at once; sharing the already-refreshed
    /// directory avoids rereading the owner account catalog once per probe while
    /// preserving the per-account credential revision check below.
    pub(crate) async fn global_runtime_with_directory(
        &self,
        account_id: global::AccountId,
        directory: &global::GlobalAccountDirectory,
    ) -> Result<Arc<GlobalAccountRuntime>, CodexErr> {
        let auth_home = directory.homes.get(&account_id).cloned().ok_or_else(|| {
            CodexErr::InvalidRequest(format!("execution account `{account_id}` is unavailable"))
        })?;
        let mut auth_config = self.auth_config_template.clone();
        auth_config.codex_home = auth_home.clone();
        auth_config.auth_credentials_store_mode =
            codex_config::types::AuthCredentialsStoreMode::File;
        let refresh = self
            .token_manager_client
            .as_ref()
            .ok_or_else(|| {
                CodexErr::InvalidRequest(format!("execution account `{account_id}` is unavailable"))
            })?
            .read_only_auth_refresh(account_id)
            .map_err(|_| {
                CodexErr::InvalidRequest(format!("execution account `{account_id}` is unavailable"))
            })?;
        let auth_manager =
            AuthManager::shared_from_read_only_auth_config_with_refresh(auth_config, refresh)
                .await
                .map_err(|_| {
                    CodexErr::InvalidRequest(format!(
                        "execution account `{account_id}` is unavailable"
                    ))
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
        account_id: global::AccountId,
        is_default: bool,
        registry_revision: u64,
        now: i64,
    ) -> AccountSlotSnapshot {
        let runtime = self.global_runtime(account_id).await.ok();
        let projection = runtime.as_ref().and_then(|runtime| {
            self.global_catalog
                .projection_for(account_id, &runtime.source_ref, now)
        });
        let quota = projection
            .as_ref()
            .filter(|projection| !projection.meters.is_empty())
            .map(|projection| {
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
                        .iter()
                        .map(|meter| AccountSlotQuotaMeter {
                            id: meter.id.clone(),
                            label: meter.label.clone(),
                            remaining_percent: meter.remaining_percent,
                            resets_at: meter.resets_at,
                        })
                        .collect(),
                    observed_at,
                    stale_at,
                }
            });
        let status = if runtime.is_some() {
            AccountSlotStatus::Ready
        } else {
            AccountSlotStatus::LoginRequired
        };
        let health = projection.as_ref().map_or_else(
            || {
                if status == AccountSlotStatus::Ready {
                    AccountSlotHealth::Healthy
                } else {
                    AccountSlotHealth::Unavailable
                }
            },
            |projection| match &projection.health {
                global::CatalogProjectionHealth::Healthy => AccountSlotHealth::Healthy,
                global::CatalogProjectionHealth::Degraded => AccountSlotHealth::Degraded,
                global::CatalogProjectionHealth::Unavailable => AccountSlotHealth::Unavailable,
            },
        );
        let active_login_operation_id = self
            .global_active_logins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&account_id)
            .cloned();
        let capability = codex_app_server_protocol::AccountSlotCapability {
            available: true,
            deny_reason: None,
        };
        let actions = crate::account_registry::live_registration::available_actions(
            status,
            &capability,
            /*is_default*/ false,
            active_login_operation_id.is_some(),
            /*active_logout*/ false,
            /*default_policy*/ None,
        );
        AccountSlotSnapshot {
            account_slot_id: account_id.to_string(),
            account_number: account_id.number(),
            label: account_id.to_string(),
            is_default,
            status,
            health,
            quota,
            auth_mode: runtime
                .as_ref()
                .and_then(|runtime| runtime.auth_manager.auth_mode())
                .map(auth_mode_to_api),
            attempt_generation: 0,
            registry_revision,
            active_login_operation_id,
            error_code: None,
            actions,
            updated_at: projection
                .as_ref()
                .map_or(0, |projection| projection.fetched_at),
        }
    }

    pub(crate) async fn global_slot_snapshot(
        &self,
        account_slot_id: &str,
    ) -> Result<AccountSlotSnapshot, JSONRPCErrorError> {
        let account_id = global::AccountId::parse(account_slot_id)
            .ok_or_else(|| invalid_request("managed account is invalid"))?;
        let directory = self.refresh_global_directory();
        if !directory.homes.contains_key(&account_id) {
            return Err(invalid_request("managed account is not registered"));
        }
        let (snapshots, _) = self
            .global_inventory_snapshot(&directory, chrono::Utc::now().timestamp())
            .await;
        snapshots
            .into_iter()
            .find(|snapshot| snapshot.account_slot_id == account_slot_id)
            .ok_or_else(|| invalid_request("managed account is unavailable"))
    }

    pub(crate) fn set_global_active_login(
        &self,
        account_slot_id: &str,
        operation_id: &str,
    ) -> bool {
        let Some(account_id) = global::AccountId::parse(account_slot_id) else {
            return false;
        };
        let mut active = self
            .global_active_logins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.contains_key(&account_id) {
            return false;
        }
        active.insert(account_id, operation_id.to_string());
        true
    }

    pub(crate) fn clear_global_active_login(&self, account_slot_id: &str, operation_id: &str) {
        let Some(account_id) = global::AccountId::parse(account_slot_id) else {
            return;
        };
        self.global_active_logins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|active_id, active| *active_id != account_id || active != operation_id);
    }

    pub(crate) async fn global_rate_limits(
        &self,
        account_slot_id: &str,
    ) -> Option<Result<AccountSlotRateLimitsReadResponse, JSONRPCErrorError>> {
        let account_id = global::AccountId::parse(account_slot_id)?;
        let projection = match self.global_runtime(account_id).await {
            Ok(runtime) => self.global_catalog.projection_for(
                account_id,
                &runtime.source_ref,
                chrono::Utc::now().timestamp(),
            ),
            Err(_) => None,
        };
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
            excluded_account_ids: &[],
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

async fn send_inventory_changed(
    outgoing: &crate::outgoing_message::OutgoingMessageSender,
    registry_revision: u64,
) {
    outgoing
        .send_server_notification(
            codex_app_server_protocol::ServerNotification::AccountSlotInventoryChanged(
                codex_app_server_protocol::AccountSlotInventoryChangedNotification {
                    registry_revision,
                },
            ),
        )
        .await;
}

async fn wait_before_catalog_reconnect(refresh_elapsed: bool) {
    if !refresh_elapsed {
        tokio::time::sleep(global::FULL_REFRESH_INTERVAL).await;
    }
}

#[cfg(test)]
#[path = "integration_tests.rs"]
mod tests;
