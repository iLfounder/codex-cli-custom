use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use chrono::Utc;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotActionAvailability;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::AccountSlotChangedNotification;
use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_app_server_protocol::AccountSlotLogoutResponse;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::JSONRPCErrorError;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
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

const SLOT_LOGIN_TERMINAL_PENDING: u8 = 0;
const SLOT_LOGIN_TERMINAL_SUCCESS: u8 = 1;
const SLOT_LOGIN_TERMINAL_FAILURE: u8 = 2;
const SLOT_LOGIN_TERMINAL_COMMITTING: u8 = 3;

#[derive(Clone, Debug)]
pub(crate) struct PreparedDefaultLogin {
    pub(crate) attempt_generation: u64,
    pub(crate) operation_id: String,
    terminal: Arc<AtomicU8>,
    cancel_requested: Arc<AtomicBool>,
}

impl PreparedDefaultLogin {
    fn try_claim_terminal(&self, terminal: u8) -> bool {
        self.terminal
            .compare_exchange(
                SLOT_LOGIN_TERMINAL_PENDING,
                terminal,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn try_claim_failure(&self) -> bool {
        self.try_claim_terminal(SLOT_LOGIN_TERMINAL_FAILURE)
    }

    pub(crate) fn try_begin_credential_commit(&self) -> bool {
        self.try_claim_terminal(SLOT_LOGIN_TERMINAL_COMMITTING)
    }

    pub(crate) fn finish_credential_commit(&self, success: bool) {
        let terminal = if success {
            SLOT_LOGIN_TERMINAL_SUCCESS
        } else {
            SLOT_LOGIN_TERMINAL_FAILURE
        };
        let _ = self.terminal.compare_exchange(
            SLOT_LOGIN_TERMINAL_COMMITTING,
            terminal,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::Release);
    }

    pub(crate) fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Acquire)
    }

    pub(crate) fn failed(&self) -> bool {
        self.terminal.load(Ordering::Acquire) == SLOT_LOGIN_TERMINAL_FAILURE
    }

    pub(crate) fn succeeded(&self) -> bool {
        self.terminal.load(Ordering::Acquire) == SLOT_LOGIN_TERMINAL_SUCCESS
    }
}

#[derive(Clone, Debug)]
pub(super) struct DefaultAccountActionPolicy {
    pub(super) login_deny_reason: Option<&'static str>,
    pub(super) logout_deny_reason: Option<&'static str>,
}

#[derive(Clone)]
pub(crate) struct PreparedSlotLogin {
    pub(crate) account_slot_id: String,
    pub(crate) attempt_generation: u64,
    pub(crate) operation_id: String,
    pub(crate) auth_home: std::path::PathBuf,
    pub(crate) runtime: Arc<AccountRuntimeBundle>,
    prior_status: ManifestSlotStatus,
    prior_error_code: Option<String>,
    terminal: Arc<AtomicU8>,
}

impl PreparedSlotLogin {
    fn try_claim_terminal(&self, terminal: u8) -> bool {
        self.terminal
            .compare_exchange(
                SLOT_LOGIN_TERMINAL_PENDING,
                terminal,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn try_claim_success(&self) -> bool {
        self.try_claim_terminal(SLOT_LOGIN_TERMINAL_SUCCESS)
    }

    pub(crate) fn try_claim_failure(&self) -> bool {
        self.try_claim_terminal(SLOT_LOGIN_TERMINAL_FAILURE)
    }
}

pub(crate) struct LoggedOutSlot {
    pub(crate) response: AccountSlotLogoutResponse,
    pub(crate) notification: AccountSlotChangedNotification,
    pub(crate) runtime: Arc<AccountRuntimeBundle>,
}

pub(crate) struct ReservedSlotLogout {
    account_slot_id: String,
    attempt_generation: u64,
    operation_id: String,
    slot: AccountSlotRecord,
    _binding_transition: OwnedMutexGuard<()>,
}

impl AccountRegistry {
    pub(crate) async fn reconcile(&self) -> Result<(), JSONRPCErrorError> {
        let reprojected_slots = self.sync_durable_runtime_projection().await;
        self.retry_manifest_projection().await;
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
            if (slot.manifest.status == ManifestSlotStatus::Failed
                && slot.manifest.error_code.as_deref() != Some(ERROR_AUTH_UNAVAILABLE)
                && !reprojected_slots.contains(&slot.manifest.account_slot_id))
                || slot.active_login_operation_id.is_some()
                || slot.active_logout_operation_id.is_some()
            {
                continue;
            }
            let Ok(binding_transition) = Arc::clone(&slot.binding_transition).try_lock_owned()
            else {
                continue;
            };
            let runtime = self.runtime(slot).await;
            runtime.auth_manager.reload().await;
            let has_auth = runtime.auth_manager.auth_cached().is_some();
            let next = match (slot.manifest.status, has_auth) {
                (ManifestSlotStatus::Ready, false) => Some((
                    ManifestSlotStatus::Failed,
                    Some(ERROR_AUTH_UNAVAILABLE.to_string()),
                )),
                (ManifestSlotStatus::LoginRequired, true) => {
                    Some((ManifestSlotStatus::Ready, None))
                }
                (ManifestSlotStatus::Failed, true) => Some((ManifestSlotStatus::Ready, None)),
                _ => None,
            };
            if let Some(next) = next {
                changed.push((
                    binding_transition,
                    slot.manifest.account_slot_id.clone(),
                    next,
                ));
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
        for (_binding_transition, slot_id, (status, error_code)) in &changed {
            if let Some(slot) = next_slots
                .iter_mut()
                .find(|slot| slot.manifest.account_slot_id == *slot_id)
                && slot.active_login_operation_id.is_none()
                && slot.active_logout_operation_id.is_none()
            {
                slot.manifest.status = *status;
                slot.manifest.error_code = error_code.clone();
                slot.manifest.updated_at = now;
                slot.active_login_operation_id = None;
                slot.active_login_cancelable = false;
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
        candidate_runtime_version: u64,
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
                if slot.active_login_operation_id.is_some() {
                    return Err(structured_invalid_request(
                        ERROR_LOGIN_BUSY,
                        "account slot login is active",
                    ));
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
                    runtime: Arc::new(super::AccountRuntimeCell::default()),
                    binding_transition: Arc::new(Mutex::new(())),
                    active_login_operation_id: None,
                    active_login_cancelable: false,
                    active_logout_operation_id: None,
                    completed_login_operation_id: None,
                });
                slots.len() - 1
            }
        };

        let slot = &mut slots[slot_index];
        slot.manifest.attempt_generation = slot.manifest.attempt_generation.saturating_add(1);
        let prior_status = slot.manifest.status;
        let prior_error_code = slot.manifest.error_code.clone();
        if prior_status != ManifestSlotStatus::Ready {
            slot.manifest.status = ManifestSlotStatus::LoginRequired;
        }
        slot.manifest.error_code = None;
        slot.manifest.updated_at = Utc::now().timestamp();
        slot.active_login_operation_id = Some(operation_id.clone());
        slot.active_login_cancelable = false;
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
        let auth_home = self.runtime_home(
            &prepared_record.manifest.account_slot_id,
            candidate_runtime_version,
        );
        let runtime = self
            .build_runtime(auth_home.clone(), candidate_runtime_version)
            .await;
        Ok(PreparedSlotLogin {
            account_slot_id: prepared_record.manifest.account_slot_id,
            attempt_generation: prepared_record.manifest.attempt_generation,
            operation_id,
            auth_home,
            runtime,
            prior_status,
            prior_error_code,
            terminal: Arc::new(AtomicU8::new(SLOT_LOGIN_TERMINAL_PENDING)),
        })
    }

    pub(crate) async fn prepare_default_login(
        &self,
        operation_id: String,
    ) -> Result<(PreparedDefaultLogin, AccountSlotChangedNotification), JSONRPCErrorError> {
        self.reconcile().await?;
        let _mutation = self.mutation_lock.lock().await;
        let (revision, mut slots) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (state.revision, state.slots.clone())
        };
        let slot = slots
            .iter_mut()
            .find(|slot| slot.manifest.is_default)
            .ok_or_else(|| internal_error("default account slot is unavailable"))?;
        slot.manifest.attempt_generation = slot.manifest.attempt_generation.saturating_add(1);
        if slot.manifest.status != ManifestSlotStatus::Ready {
            slot.manifest.status = ManifestSlotStatus::LoginRequired;
        }
        slot.manifest.error_code = None;
        slot.manifest.updated_at = Utc::now().timestamp();
        slot.active_login_operation_id = Some(operation_id.clone());
        slot.active_login_cancelable = false;
        slot.completed_login_operation_id = None;
        let attempt_generation = slot.manifest.attempt_generation;
        let next_revision = revision.saturating_add(1);
        self.persist_slots(next_revision, &slots)?;
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
        let prepared = PreparedDefaultLogin {
            attempt_generation,
            operation_id,
            terminal: Arc::new(AtomicU8::new(SLOT_LOGIN_TERMINAL_PENDING)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        };
        let notification = self.changed_notification(super::DEFAULT_SLOT_ID).await?;
        Ok((prepared, notification))
    }

    pub(crate) async fn mark_login_cancelable(
        &self,
        account_slot_id: &str,
        attempt_generation: u64,
        operation_id: &str,
    ) -> Result<AccountSlotChangedNotification, JSONRPCErrorError> {
        let _mutation = self.mutation_lock.lock().await;
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
                slot.manifest.account_slot_id == account_slot_id
                    && slot.manifest.attempt_generation == attempt_generation
                    && slot.active_login_operation_id.as_deref() == Some(operation_id)
            })
            .ok_or_else(|| {
                structured_invalid_request(
                    ERROR_LOGIN_CANCELED,
                    "account slot login was superseded",
                )
            })?;
        slot.active_login_cancelable = true;
        let next_revision = revision.saturating_add(1);
        self.persist_slots(next_revision, &slots)?;
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
        self.changed_notification(account_slot_id).await
    }

    pub(crate) async fn finish_default_login(
        &self,
        prepared: &PreparedDefaultLogin,
        success: bool,
        error_code: Option<&str>,
    ) -> Result<Option<AccountSlotChangedNotification>, JSONRPCErrorError> {
        let _mutation = self.mutation_lock.lock().await;
        let (revision, mut slots) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (state.revision, state.slots.clone())
        };
        let Some(slot) = slots.iter_mut().find(|slot| {
            slot.manifest.is_default
                && slot.manifest.attempt_generation == prepared.attempt_generation
                && slot.active_login_operation_id.as_deref() == Some(&prepared.operation_id)
        }) else {
            return Ok(None);
        };
        let runtime = slot
            .runtime
            .get()
            .ok_or_else(|| internal_error("default account runtime is unavailable"))?;
        let has_auth = runtime.auth_manager.auth_cached().is_some();
        let committed_success = success && has_auth;
        slot.manifest.status = if committed_success || has_auth {
            ManifestSlotStatus::Ready
        } else {
            ManifestSlotStatus::Failed
        };
        slot.manifest.error_code = if committed_success {
            None
        } else if success {
            Some(ERROR_AUTH_UNAVAILABLE.to_string())
        } else {
            error_code.map(str::to_string)
        };
        slot.manifest.updated_at = Utc::now().timestamp();
        slot.active_login_operation_id = None;
        slot.active_login_cancelable = false;
        slot.completed_login_operation_id =
            committed_success.then(|| prepared.operation_id.clone());
        let next_revision = revision.saturating_add(1);
        let projection_error = self.persist_slots(next_revision, &slots).err();
        if let Some(error) = projection_error.as_ref() {
            if prepared.cancel_requested() {
                return Err(internal_error(format!(
                    "failed to persist default account login cancellation: {}",
                    error.message
                )));
            }
            tracing::warn!(
                login_id = %prepared.operation_id,
                attempt_generation = prepared.attempt_generation,
                "default account login completed; account-slots.toml projection will be retried: {}",
                error.message
            );
        }
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            state.revision = next_revision;
            state.slots = slots;
            state.manifest_present = true;
            state.projection_dirty = projection_error.is_some();
        }
        drop(_mutation);
        self.changed_notification(super::DEFAULT_SLOT_ID)
            .await
            .map(Some)
    }

    pub(crate) async fn refresh_default_projection(
        &self,
    ) -> Result<AccountSlotChangedNotification, JSONRPCErrorError> {
        let _mutation = self.mutation_lock.lock().await;
        let (revision, mut slots) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (state.revision, state.slots.clone())
        };
        let slot = slots
            .iter_mut()
            .find(|slot| slot.manifest.is_default)
            .ok_or_else(|| internal_error("default account slot is unavailable"))?;
        let runtime = slot
            .runtime
            .get()
            .ok_or_else(|| internal_error("default account runtime is unavailable"))?;
        slot.manifest.status = if runtime.auth_manager.auth_cached().is_some() {
            ManifestSlotStatus::Ready
        } else {
            ManifestSlotStatus::LoginRequired
        };
        let login_active = slot.active_login_operation_id.is_some();
        if !login_active {
            slot.manifest.error_code = None;
            slot.completed_login_operation_id = None;
        }
        slot.manifest.updated_at = Utc::now().timestamp();
        let next_revision = revision.saturating_add(1);
        self.persist_slots(next_revision, &slots)?;
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
        self.changed_notification(super::DEFAULT_SLOT_ID).await
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
        let published_runtime = if status == ManifestSlotStatus::Ready {
            slot.manifest.status = status;
            slot.manifest.auth_home = prepared.auth_home.clone();
            slot.manifest.error_code = None;
            Some((Arc::clone(&slot.runtime), Arc::clone(&prepared.runtime)))
        } else if prepared.prior_status == ManifestSlotStatus::Ready {
            slot.manifest.status = prepared.prior_status;
            slot.manifest.error_code = error_code
                .map(str::to_string)
                .or_else(|| prepared.prior_error_code.clone());
            None
        } else {
            slot.manifest.status = status;
            slot.manifest.error_code = error_code.map(str::to_string);
            None
        };
        slot.manifest.updated_at = Utc::now().timestamp();
        slot.completed_login_operation_id =
            (status == ManifestSlotStatus::Ready).then(|| prepared.operation_id.clone());
        slot.active_login_operation_id = None;
        slot.active_login_cancelable = false;
        let changed_slot_id = slot.manifest.account_slot_id.clone();
        let next_revision = revision.saturating_add(1);
        let projection_error = self.persist_slots(next_revision, &slots).err();
        if let Some(error) = projection_error.as_ref() {
            if status != ManifestSlotStatus::Ready {
                return Err(internal_error(format!(
                    "failed to persist account slot projection: {}",
                    error.message
                )));
            }
            tracing::warn!(
                account_slot_id = %prepared.account_slot_id,
                runtime_version = prepared.runtime.runtime_version.load(std::sync::atomic::Ordering::Acquire),
                "durable account runtime committed; account-slots.toml projection will be retried: {}",
                error.message
            );
        }
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            state.revision = next_revision;
            state.slots = slots;
            state.projection_dirty = projection_error.is_some();
        }
        if let Some((runtime_cell, runtime)) = published_runtime {
            runtime_cell.replace(runtime);
        }
        drop(mutation);
        self.changed_notification(&changed_slot_id).await.map(Some)
    }

    async fn retry_manifest_projection(&self) {
        let mutation = self.mutation_lock.lock().await;
        let (revision, slots, dirty) = match self.state.read() {
            Ok(state) => (state.revision, state.slots.clone(), state.projection_dirty),
            Err(_) => return,
        };
        if !dirty {
            return;
        }
        if let Err(error) = self.persist_slots(revision, &slots) {
            tracing::warn!(
                "account-slots.toml recovery projection remains pending: {}",
                error.message
            );
            return;
        }
        if let Ok(mut state) = self.state.write()
            && state.revision == revision
        {
            state.projection_dirty = false;
        }
        drop(mutation);
    }

    async fn sync_durable_runtime_projection(&self) -> HashSet<String> {
        let slots = match self.state.read() {
            Ok(state) => state.slots.clone(),
            Err(_) => return HashSet::new(),
        };
        let mut durable = Vec::new();
        for slot in slots.iter().filter(|slot| !slot.manifest.is_default) {
            match self
                .thread_store
                .execution_account_slot_runtime_state(slot.manifest.account_slot_id.clone())
                .await
            {
                Ok((runtime_version, _)) if runtime_version > 0 => durable.push((
                    slot.manifest.account_slot_id.clone(),
                    self.runtime_home(&slot.manifest.account_slot_id, runtime_version),
                )),
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    account_slot_id = %slot.manifest.account_slot_id,
                    "failed to reconcile durable account runtime projection: {error}"
                ),
            }
        }
        if durable.is_empty() {
            return HashSet::new();
        }
        let _mutation = self.mutation_lock.lock().await;
        let mut state = match self.state.write() {
            Ok(state) => state,
            Err(_) => return HashSet::new(),
        };
        let mut changed = false;
        let mut reprojected_slots = HashSet::new();
        for (slot_id, auth_home) in durable {
            if let Some(slot) = state
                .slots
                .iter_mut()
                .find(|slot| slot.manifest.account_slot_id == slot_id)
                && slot.manifest.auth_home != auth_home
            {
                slot.manifest.auth_home = auth_home;
                reprojected_slots.insert(slot_id);
                changed = true;
            }
        }
        if changed {
            state.revision = state.revision.saturating_add(1);
            state.projection_dirty = true;
        }
        reprojected_slots
    }

    #[cfg(test)]
    pub(crate) async fn logout_secondary(
        &self,
        params: AccountSlotLogoutParams,
    ) -> Result<LoggedOutSlot, JSONRPCErrorError> {
        let reservation = self.reserve_secondary_logout(params).await?;
        self.logout_reserved_secondary(reservation).await
    }

    pub(crate) async fn logout_reserved_secondary(
        &self,
        reservation: ReservedSlotLogout,
    ) -> Result<LoggedOutSlot, JSONRPCErrorError> {
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

    pub(crate) async fn reserve_secondary_logout(
        &self,
        params: AccountSlotLogoutParams,
    ) -> Result<ReservedSlotLogout, JSONRPCErrorError> {
        self.reconcile().await?;
        let binding_transition = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            let slot = state
                .slots
                .iter()
                .find(|slot| slot.manifest.account_slot_id == params.account_slot_id)
                .ok_or_else(|| invalid_request("account slot is unavailable"))?;
            if slot.active_logout_operation_id.is_some() {
                return Err(structured_invalid_request(
                    ERROR_LOGOUT_BUSY,
                    "account slot logout is active",
                ));
            }
            Arc::clone(&slot.binding_transition)
        };
        let binding_transition = binding_transition.lock_owned().await;
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
            _binding_transition: binding_transition,
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

    pub(crate) async fn clear_logout_reservation(
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
        let binding_transition = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            let Some(slot) = state.slots.iter().find(|slot| {
                slot.manifest.account_slot_id == prepared.account_slot_id
                    && slot.manifest.attempt_generation == prepared.attempt_generation
                    && slot.manifest.status == ManifestSlotStatus::Ready
                    && slot.completed_login_operation_id.as_deref() == Some(&prepared.operation_id)
            }) else {
                return Ok(None);
            };
            Arc::clone(&slot.binding_transition)
        };
        let _binding_transition = binding_transition.lock_owned().await;
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
        let current_config = self.load_latest_config().await;
        Ok(self
            .snapshot(&slot, revision, &capability, &current_config)
            .await)
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

    pub(crate) async fn changed_notification(
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
    status: AccountSlotStatus,
    capability: &AccountSlotCapability,
    is_default: bool,
    active_login: bool,
    active_logout: bool,
    default_policy: Option<&DefaultAccountActionPolicy>,
) -> Vec<AccountSlotActionAvailability> {
    let unavailable = |action, reason: &str| AccountSlotActionAvailability {
        action,
        allowed: false,
        deny_reason: Some(reason.to_string()),
    };
    if active_login {
        return vec![
            unavailable(AccountSlotAction::Login, ERROR_LOGIN_BUSY),
            unavailable(AccountSlotAction::RetryLogin, ERROR_LOGIN_BUSY),
            unavailable(AccountSlotAction::SwitchTo, ERROR_LOGIN_BUSY),
            unavailable(AccountSlotAction::Logout, ERROR_LOGIN_BUSY),
        ];
    }
    if active_logout {
        return vec![
            unavailable(AccountSlotAction::Login, ERROR_LOGOUT_BUSY),
            unavailable(AccountSlotAction::RetryLogin, ERROR_LOGOUT_BUSY),
            unavailable(AccountSlotAction::SwitchTo, ERROR_LOGOUT_BUSY),
            unavailable(AccountSlotAction::Logout, ERROR_LOGOUT_BUSY),
        ];
    }
    if is_default {
        let unavailable_policy = DefaultAccountActionPolicy {
            login_deny_reason: Some(DENY_LOGIN_NOT_AVAILABLE),
            logout_deny_reason: Some(DENY_LOGIN_NOT_AVAILABLE),
        };
        let policy = default_policy.unwrap_or(&unavailable_policy);
        return vec![
            AccountSlotActionAvailability {
                action: AccountSlotAction::Login,
                allowed: status == AccountSlotStatus::LoginRequired
                    && policy.login_deny_reason.is_none(),
                deny_reason: if status != AccountSlotStatus::LoginRequired {
                    Some(DENY_LOGIN_NOT_AVAILABLE.to_string())
                } else {
                    policy.login_deny_reason.map(str::to_string)
                },
            },
            AccountSlotActionAvailability {
                action: AccountSlotAction::RetryLogin,
                allowed: matches!(status, AccountSlotStatus::Ready | AccountSlotStatus::Failed)
                    && policy.login_deny_reason.is_none(),
                deny_reason: if !matches!(
                    status,
                    AccountSlotStatus::Ready | AccountSlotStatus::Failed
                ) {
                    Some(DENY_LOGIN_NOT_AVAILABLE.to_string())
                } else {
                    policy.login_deny_reason.map(str::to_string)
                },
            },
            AccountSlotActionAvailability {
                action: AccountSlotAction::SwitchTo,
                allowed: status == AccountSlotStatus::Ready,
                deny_reason: (status != AccountSlotStatus::Ready)
                    .then(|| super::DENY_SWITCH_NOT_AVAILABLE.to_string()),
            },
            AccountSlotActionAvailability {
                action: AccountSlotAction::Logout,
                allowed: status == AccountSlotStatus::Ready && policy.logout_deny_reason.is_none(),
                deny_reason: if status != AccountSlotStatus::Ready {
                    Some(DENY_LOGIN_NOT_AVAILABLE.to_string())
                } else {
                    policy.logout_deny_reason.map(str::to_string)
                },
            },
        ];
    }
    if let Some(reason) = capability.deny_reason.as_deref() {
        return vec![
            unavailable(AccountSlotAction::Login, reason),
            unavailable(AccountSlotAction::RetryLogin, reason),
            unavailable(AccountSlotAction::SwitchTo, reason),
            unavailable(AccountSlotAction::Logout, reason),
        ];
    }
    vec![
        AccountSlotActionAvailability {
            action: AccountSlotAction::Login,
            allowed: status == AccountSlotStatus::LoginRequired,
            deny_reason: (status != AccountSlotStatus::LoginRequired)
                .then(|| DENY_LOGIN_NOT_AVAILABLE.to_string()),
        },
        AccountSlotActionAvailability {
            action: AccountSlotAction::Logout,
            allowed: status == AccountSlotStatus::Ready,
            deny_reason: (status != AccountSlotStatus::Ready)
                .then(|| DENY_LOGIN_NOT_AVAILABLE.to_string()),
        },
        AccountSlotActionAvailability {
            action: AccountSlotAction::RetryLogin,
            allowed: matches!(status, AccountSlotStatus::Ready | AccountSlotStatus::Failed),
            deny_reason: (!matches!(status, AccountSlotStatus::Ready | AccountSlotStatus::Failed))
                .then(|| DENY_LOGIN_NOT_AVAILABLE.to_string()),
        },
        AccountSlotActionAvailability {
            action: AccountSlotAction::SwitchTo,
            allowed: status == AccountSlotStatus::Ready,
            deny_reason: (status != AccountSlotStatus::Ready)
                .then(|| super::DENY_SWITCH_NOT_AVAILABLE.to_string()),
        },
    ]
}

pub(crate) fn structured_invalid_request(code: &str, message: &str) -> JSONRPCErrorError {
    let mut error = invalid_request(message);
    error.data = Some(serde_json::json!({ "reason": code }));
    error
}
