use std::sync::Arc;

use chrono::Utc;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotActionAvailability;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::AccountSlotChangedNotification;
use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_app_server_protocol::AccountSlotLogoutResponse;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::JSONRPCErrorError;
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::AccountRegistry;
use super::AccountRuntimeBundle;
use super::AccountSlotManifest;
use super::AccountSlotRecord;
use super::AccountSlotsManifest;
use super::BrowserLoginOwner;
use super::DENY_LOGIN_NOT_AVAILABLE;
use super::MANIFEST_FILE;
use super::MANIFEST_SCHEMA_VERSION;
use super::ManifestSlotStatus;
use super::PRIVATE_HOMES_DIR;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;

pub(crate) const ERROR_AUTH_UNAVAILABLE: &str = "authUnavailable";
pub(crate) const ERROR_BROWSER_LOGIN_BUSY: &str = "browserLoginBusy";
pub(crate) const ERROR_LOGIN_BUSY: &str = "accountSlotLoginBusy";
pub(crate) const ERROR_LOGIN_CANCELED: &str = "loginCanceled";
pub(crate) const ERROR_LOGIN_FAILED: &str = "loginFailed";
pub(crate) const ERROR_LOGOUT_BUSY: &str = "accountSlotLogoutBusy";
pub(crate) const ERROR_REFRESH_UNAVAILABLE: &str = "refreshUnavailable";

#[derive(Clone)]
pub(crate) struct PreparedSlotLogin {
    pub(crate) account_slot_id: String,
    pub(crate) attempt_generation: u64,
    pub(crate) operation_id: String,
    pub(crate) auth_home: std::path::PathBuf,
    pub(crate) runtime: Arc<AccountRuntimeBundle>,
}

pub(crate) struct LoggedOutSlot {
    pub(crate) response: AccountSlotLogoutResponse,
    pub(crate) notification: AccountSlotChangedNotification,
    pub(crate) runtime: Arc<AccountRuntimeBundle>,
}

pub(super) struct ReservedSlotLogout {
    account_slot_id: String,
    attempt_generation: u64,
    operation_id: String,
    slot: AccountSlotRecord,
}

impl AccountRegistry {
    pub(crate) async fn reconcile(&self) -> Result<(), JSONRPCErrorError> {
        let (revision, slots, manifest_error, manifest_present) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (
                state.revision,
                state.slots.clone(),
                state.manifest_error,
                state.manifest_present,
            )
        };
        if manifest_error.is_some() || !manifest_present {
            return Ok(());
        }

        let mut changed = Vec::new();
        for slot in &slots {
            if slot.manifest.status == ManifestSlotStatus::Failed
                || slot.active_logout_operation_id.is_some()
            {
                continue;
            }
            let runtime = self.runtime(slot).await;
            let has_auth = runtime.auth_manager.auth().await.is_some();
            let next = match (slot.manifest.status, has_auth) {
                (ManifestSlotStatus::Ready, false) => Some((
                    ManifestSlotStatus::Failed,
                    Some(ERROR_AUTH_UNAVAILABLE.to_string()),
                )),
                (ManifestSlotStatus::LoginRequired, true) => {
                    Some((ManifestSlotStatus::Ready, None))
                }
                _ => None,
            };
            if let Some(next) = next {
                changed.push((slot.manifest.account_slot_id.clone(), next));
            }
        }
        if changed.is_empty() {
            return Ok(());
        }

        let _mutation = self.mutation_lock.lock().await;
        let mut next_slots = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            if state.revision != revision {
                return Ok(());
            }
            state.slots.clone()
        };
        let next_revision = revision.saturating_add(1);
        let now = Utc::now().timestamp();
        let mut applied = false;
        for (slot_id, (status, error_code)) in changed {
            if let Some(slot) = next_slots
                .iter_mut()
                .find(|slot| slot.manifest.account_slot_id == slot_id)
                && slot.active_logout_operation_id.is_none()
            {
                slot.manifest.status = status;
                slot.manifest.error_code = error_code;
                slot.manifest.updated_at = now;
                slot.active_login_operation_id = None;
                slot.completed_login_operation_id = None;
                applied = true;
            }
        }
        if !applied {
            return Ok(());
        }
        self.persist_slots(next_revision, &next_slots)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| internal_error("account slot registry is unavailable"))?;
        state.revision = next_revision;
        state.slots = next_slots;
        Ok(())
    }

    pub(crate) async fn prepare_slot_login(
        &self,
        requested_slot_id: Option<String>,
        operation_id: String,
    ) -> Result<PreparedSlotLogin, JSONRPCErrorError> {
        if requested_slot_id.is_none()
            && self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?
                .slots
                .len()
                >= super::MAX_MANIFEST_SLOTS
        {
            return Err(invalid_request("account slot limit has been reached"));
        }
        self.reconcile().await?;
        let _mutation = self.mutation_lock.lock().await;
        let (revision, mut slots, manifest_error) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (state.revision, state.slots.clone(), state.manifest_error)
        };
        let capability = self.capability(manifest_error);
        if !capability.available {
            return Err(structured_invalid_request(
                capability
                    .deny_reason
                    .as_deref()
                    .unwrap_or(DENY_LOGIN_NOT_AVAILABLE),
                "multi-account login is unavailable",
            ));
        }

        let slot_index = match requested_slot_id {
            Some(slot_id) => {
                let index = slots
                    .iter()
                    .position(|slot| slot.manifest.account_slot_id == slot_id)
                    .ok_or_else(|| invalid_request("account slot is unavailable"))?;
                let slot = &slots[index];
                if slot.manifest.is_default {
                    return Err(invalid_request(
                        "default account login must use account/login/start",
                    ));
                }
                if slot.active_logout_operation_id.is_some() {
                    return Err(structured_invalid_request(
                        ERROR_LOGOUT_BUSY,
                        "account slot logout is active",
                    ));
                }
                if slot.manifest.status == ManifestSlotStatus::Ready {
                    return Err(invalid_request("account slot is already ready"));
                }
                index
            }
            None => {
                if slots.len() >= super::MAX_MANIFEST_SLOTS {
                    return Err(invalid_request("account slot limit has been reached"));
                }
                let account_slot_id = Uuid::new_v4().simple().to_string();
                let ordinal = slots
                    .iter()
                    .filter(|slot| !slot.manifest.is_default)
                    .count()
                    + 2;
                slots.push(AccountSlotRecord {
                    manifest: AccountSlotManifest {
                        auth_home: self
                            .config
                            .codex_home
                            .join(PRIVATE_HOMES_DIR)
                            .join(&account_slot_id)
                            .to_path_buf(),
                        account_slot_id,
                        label: format!("Account {ordinal}"),
                        is_default: false,
                        status: ManifestSlotStatus::LoginRequired,
                        attempt_generation: 0,
                        updated_at: 0,
                        error_code: None,
                    },
                    runtime: Arc::new(OnceCell::new()),
                    active_login_operation_id: None,
                    active_logout_operation_id: None,
                    completed_login_operation_id: None,
                });
                slots.len() - 1
            }
        };

        let slot = &mut slots[slot_index];
        slot.manifest.attempt_generation = slot.manifest.attempt_generation.saturating_add(1);
        slot.manifest.status = ManifestSlotStatus::LoginRequired;
        slot.manifest.error_code = None;
        slot.manifest.updated_at = Utc::now().timestamp();
        slot.active_login_operation_id = Some(operation_id.clone());
        slot.completed_login_operation_id = None;
        let next_revision = revision.saturating_add(1);
        self.persist_slots(next_revision, &slots)?;

        let prepared_record = slots[slot_index].clone();
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            state.revision = next_revision;
            state.slots = slots;
            state.manifest_present = true;
        }
        drop(_mutation);
        let runtime = self.runtime(&prepared_record).await;
        Ok(PreparedSlotLogin {
            account_slot_id: prepared_record.manifest.account_slot_id,
            attempt_generation: prepared_record.manifest.attempt_generation,
            operation_id,
            auth_home: prepared_record.manifest.auth_home,
            runtime,
        })
    }

    pub(crate) async fn finish_slot_login(
        &self,
        prepared: &PreparedSlotLogin,
        status: ManifestSlotStatus,
        error_code: Option<&str>,
    ) -> Result<Option<AccountSlotChangedNotification>, JSONRPCErrorError> {
        let mutation = self.mutation_lock.lock().await;
        let (revision, mut slots) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (state.revision, state.slots.clone())
        };
        let Some(slot) = slots.iter_mut().find(|slot| {
            slot.manifest.account_slot_id == prepared.account_slot_id
                && slot.manifest.attempt_generation == prepared.attempt_generation
                && slot.active_login_operation_id.as_deref() == Some(&prepared.operation_id)
        }) else {
            return Ok(None);
        };
        slot.manifest.status = status;
        slot.manifest.error_code = error_code.map(str::to_string);
        slot.manifest.updated_at = Utc::now().timestamp();
        slot.completed_login_operation_id =
            (status == ManifestSlotStatus::Ready).then(|| prepared.operation_id.clone());
        slot.active_login_operation_id = None;
        let changed_slot_id = slot.manifest.account_slot_id.clone();
        let next_revision = revision.saturating_add(1);
        self.persist_slots(next_revision, &slots)?;
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            state.revision = next_revision;
            state.slots = slots;
        }
        drop(mutation);
        self.changed_notification(&changed_slot_id).await.map(Some)
    }

    pub(crate) async fn logout_secondary(
        &self,
        params: AccountSlotLogoutParams,
    ) -> Result<LoggedOutSlot, JSONRPCErrorError> {
        self.reconcile().await?;
        let reservation = self.reserve_secondary_logout(params).await?;
        let runtime = self.runtime(&reservation.slot).await;
        if runtime.auth_manager.logout_with_revoke().await.is_err() {
            self.clear_logout_reservation(&reservation).await?;
            return Err(internal_error("account slot logout failed"));
        }
        let result = self.finish_secondary_logout(&reservation).await;
        if result.is_err() {
            self.clear_logout_reservation(&reservation).await?;
        }
        let notification = result?;
        Ok(LoggedOutSlot {
            response: AccountSlotLogoutResponse {
                slot: notification.slot.clone(),
            },
            notification,
            runtime,
        })
    }

    pub(super) async fn reserve_secondary_logout(
        &self,
        params: AccountSlotLogoutParams,
    ) -> Result<ReservedSlotLogout, JSONRPCErrorError> {
        let _mutation = self.mutation_lock.lock().await;
        let (revision, manifest_error) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (state.revision, state.manifest_error)
        };
        let capability = self.capability(manifest_error);
        if !capability.available {
            return Err(structured_invalid_request(
                capability
                    .deny_reason
                    .as_deref()
                    .unwrap_or(DENY_LOGIN_NOT_AVAILABLE),
                "multi-account logout is unavailable",
            ));
        }
        if revision != params.expected_registry_revision {
            return Err(invalid_request("account slot registry revision is stale"));
        }
        let operation_id = Uuid::new_v4().to_string();
        let mut state = self
            .state
            .write()
            .map_err(|_| internal_error("account slot registry is unavailable"))?;
        let slot = state
            .slots
            .iter_mut()
            .find(|slot| slot.manifest.account_slot_id == params.account_slot_id)
            .ok_or_else(|| invalid_request("account slot is unavailable"))?;
        if slot.manifest.is_default {
            return Err(invalid_request(
                "default account logout must use account/logout",
            ));
        }
        if slot.manifest.attempt_generation != params.expected_attempt_generation {
            return Err(invalid_request("account slot attempt generation is stale"));
        }
        if slot.active_login_operation_id.is_some() {
            return Err(structured_invalid_request(
                ERROR_LOGIN_BUSY,
                "account slot login is active",
            ));
        }
        if slot.active_logout_operation_id.is_some() {
            return Err(structured_invalid_request(
                ERROR_LOGOUT_BUSY,
                "account slot logout is active",
            ));
        }
        if slot.manifest.status != ManifestSlotStatus::Ready {
            return Err(invalid_request("account slot is not ready"));
        }
        slot.active_logout_operation_id = Some(operation_id.clone());
        Ok(ReservedSlotLogout {
            account_slot_id: slot.manifest.account_slot_id.clone(),
            attempt_generation: slot.manifest.attempt_generation,
            operation_id,
            slot: slot.clone(),
        })
    }

    async fn finish_secondary_logout(
        &self,
        reservation: &ReservedSlotLogout,
    ) -> Result<AccountSlotChangedNotification, JSONRPCErrorError> {
        let mutation = self.mutation_lock.lock().await;
        let (revision, mut slots) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (state.revision, state.slots.clone())
        };
        let slot = slots
            .iter_mut()
            .find(|slot| {
                slot.manifest.account_slot_id == reservation.account_slot_id
                    && slot.manifest.attempt_generation == reservation.attempt_generation
                    && slot.active_logout_operation_id.as_deref() == Some(&reservation.operation_id)
            })
            .ok_or_else(|| internal_error("account slot logout reservation was superseded"))?;

        slot.manifest.attempt_generation = slot.manifest.attempt_generation.saturating_add(1);
        slot.manifest.status = ManifestSlotStatus::LoginRequired;
        slot.manifest.error_code = None;
        slot.manifest.updated_at = Utc::now().timestamp();
        slot.active_logout_operation_id = None;
        slot.completed_login_operation_id = None;
        let slot_id = slot.manifest.account_slot_id.clone();
        let next_revision = revision.saturating_add(1);
        self.persist_slots(next_revision, &slots)?;
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            state.revision = next_revision;
            state.slots = slots;
        }
        drop(mutation);
        self.changed_notification(&slot_id).await
    }

    pub(super) async fn clear_logout_reservation(
        &self,
        reservation: &ReservedSlotLogout,
    ) -> Result<(), JSONRPCErrorError> {
        let _mutation = self.mutation_lock.lock().await;
        let mut state = self
            .state
            .write()
            .map_err(|_| internal_error("account slot registry is unavailable"))?;
        if let Some(slot) = state.slots.iter_mut().find(|slot| {
            slot.manifest.account_slot_id == reservation.account_slot_id
                && slot.manifest.attempt_generation == reservation.attempt_generation
                && slot.active_logout_operation_id.as_deref() == Some(&reservation.operation_id)
        }) {
            slot.active_logout_operation_id = None;
        }
        Ok(())
    }

    pub(crate) async fn fail_ready_slot(
        &self,
        prepared: &PreparedSlotLogin,
        error_code: &'static str,
    ) -> Result<Option<AccountSlotChangedNotification>, JSONRPCErrorError> {
        let mutation = self.mutation_lock.lock().await;
        let (revision, mut slots) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (state.revision, state.slots.clone())
        };
        let Some(slot) = slots.iter_mut().find(|slot| {
            slot.manifest.account_slot_id == prepared.account_slot_id
                && slot.manifest.attempt_generation == prepared.attempt_generation
                && slot.manifest.status == ManifestSlotStatus::Ready
                && slot.completed_login_operation_id.as_deref() == Some(&prepared.operation_id)
        }) else {
            return Ok(None);
        };
        slot.manifest.status = ManifestSlotStatus::Failed;
        slot.manifest.error_code = Some(error_code.to_string());
        slot.manifest.updated_at = Utc::now().timestamp();
        slot.completed_login_operation_id = None;
        let changed_slot_id = slot.manifest.account_slot_id.clone();
        let next_revision = revision.saturating_add(1);
        self.persist_slots(next_revision, &slots)?;
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            state.revision = next_revision;
            state.slots = slots;
        }
        drop(mutation);
        self.changed_notification(&changed_slot_id).await.map(Some)
    }

    pub(crate) async fn slot_snapshot(
        &self,
        slot_id: &str,
    ) -> Result<AccountSlotSnapshot, JSONRPCErrorError> {
        let (revision, slot, manifest_error) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            let slot = state
                .slots
                .iter()
                .find(|slot| slot.manifest.account_slot_id == slot_id)
                .cloned()
                .ok_or_else(|| invalid_request("account slot is unavailable"))?;
            (state.revision, slot, state.manifest_error)
        };
        let capability = self.capability(manifest_error);
        Ok(self.snapshot(&slot, revision, &capability).await)
    }

    pub(crate) async fn try_begin_browser_login(
        &self,
        owner: BrowserLoginOwner,
    ) -> Result<(), JSONRPCErrorError> {
        let mut active = self
            .browser_login
            .lock()
            .map_err(|_| internal_error("browser login coordinator is unavailable"))?;
        if active.is_some() {
            return Err(structured_invalid_request(
                ERROR_BROWSER_LOGIN_BUSY,
                "a browser login is already active",
            ));
        }
        *active = Some(owner);
        Ok(())
    }

    pub(crate) async fn finish_browser_login(&self, owner: &BrowserLoginOwner) {
        if let Ok(mut active) = self.browser_login.lock()
            && active.as_ref() == Some(owner)
        {
            *active = None;
        }
    }

    fn persist_slots(
        &self,
        revision: u64,
        slots: &[AccountSlotRecord],
    ) -> Result<(), JSONRPCErrorError> {
        AccountSlotsManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            revision,
            slots: slots.iter().map(|slot| slot.manifest.clone()).collect(),
        }
        .persist(&self.config.codex_home.join(MANIFEST_FILE))
        .map_err(|_| internal_error("account slot manifest could not be persisted"))
    }

    async fn changed_notification(
        &self,
        slot_id: &str,
    ) -> Result<AccountSlotChangedNotification, JSONRPCErrorError> {
        let slot = self.slot_snapshot(slot_id).await?;
        Ok(AccountSlotChangedNotification {
            registry_revision: slot.registry_revision,
            slot,
        })
    }
}

pub(super) fn available_actions(
    status: ManifestSlotStatus,
    capability: &AccountSlotCapability,
    is_default: bool,
    active_operation: bool,
) -> Vec<AccountSlotActionAvailability> {
    let unavailable = |action, reason: &str| AccountSlotActionAvailability {
        action,
        allowed: false,
        deny_reason: Some(reason.to_string()),
    };
    if let Some(reason) = capability.deny_reason.as_deref() {
        return vec![
            unavailable(AccountSlotAction::Login, reason),
            unavailable(AccountSlotAction::RetryLogin, reason),
            unavailable(AccountSlotAction::SwitchTo, reason),
            unavailable(AccountSlotAction::Logout, reason),
        ];
    }
    if is_default {
        return vec![
            unavailable(AccountSlotAction::Login, DENY_LOGIN_NOT_AVAILABLE),
            unavailable(AccountSlotAction::RetryLogin, DENY_LOGIN_NOT_AVAILABLE),
            unavailable(
                AccountSlotAction::SwitchTo,
                super::DENY_SWITCH_NOT_AVAILABLE,
            ),
            unavailable(AccountSlotAction::Logout, DENY_LOGIN_NOT_AVAILABLE),
        ];
    }
    if active_operation {
        return vec![
            unavailable(AccountSlotAction::Login, ERROR_LOGIN_BUSY),
            unavailable(AccountSlotAction::RetryLogin, ERROR_LOGIN_BUSY),
            unavailable(AccountSlotAction::SwitchTo, ERROR_LOGIN_BUSY),
            unavailable(AccountSlotAction::Logout, ERROR_LOGIN_BUSY),
        ];
    }
    vec![
        AccountSlotActionAvailability {
            action: AccountSlotAction::Login,
            allowed: status == ManifestSlotStatus::LoginRequired,
            deny_reason: (status != ManifestSlotStatus::LoginRequired)
                .then(|| DENY_LOGIN_NOT_AVAILABLE.to_string()),
        },
        AccountSlotActionAvailability {
            action: AccountSlotAction::Logout,
            allowed: status == ManifestSlotStatus::Ready,
            deny_reason: (status != ManifestSlotStatus::Ready)
                .then(|| DENY_LOGIN_NOT_AVAILABLE.to_string()),
        },
        AccountSlotActionAvailability {
            action: AccountSlotAction::RetryLogin,
            allowed: status == ManifestSlotStatus::Failed,
            deny_reason: (status != ManifestSlotStatus::Failed)
                .then(|| DENY_LOGIN_NOT_AVAILABLE.to_string()),
        },
        AccountSlotActionAvailability {
            action: AccountSlotAction::SwitchTo,
            allowed: false,
            deny_reason: Some(super::DENY_SWITCH_NOT_AVAILABLE.to_string()),
        },
    ]
}

pub(crate) fn structured_invalid_request(code: &str, message: &str) -> JSONRPCErrorError {
    let mut error = invalid_request(message);
    error.data = Some(serde_json::json!({ "reason": code }));
    error
}
