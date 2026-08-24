use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::AccountSlotListParams;
use codex_app_server_protocol::AccountSlotListResponse;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::ExecutionAccountContext;
use codex_core::ExecutionAccountResolver;
use codex_core::ExecutionAccountResolverFuture;
use codex_core::config::Config;
use codex_core::execution_account::ExecutionAccountTransitionResolverFuture;
use codex_core::execution_account::ResolvedExecutionAccountTransition;
use codex_core::path_utils::write_atomically;
use codex_login::AuthConfig;
use codex_login::AuthManager;
use codex_login::AuthSourceKind;
use codex_model_provider::create_model_provider;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_thread_store::ThreadStore;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::auth_mode::auth_mode_to_api;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;

const MANIFEST_FILE: &str = "account-slots.toml";
const PRIVATE_HOMES_DIR: &str = "accounts";
const MANIFEST_SCHEMA_VERSION: u32 = 1;
const DEFAULT_SLOT_ID: &str = "default";
const DEFAULT_LIST_LIMIT: usize = 50;
const MAX_LIST_LIMIT: usize = 100;
const MAX_MANIFEST_SLOTS: usize = 1_000;

const DENY_MANIFEST_INVALID: &str = "account_slot_manifest_invalid";
const DENY_HOST_ACCESS_TOKEN: &str = "host_owned_codex_access_token";
const DENY_HOST_EXTERNAL_AUTH: &str = "host_owned_external_auth";
const DENY_HOST_PROVIDER_AUTH: &str = "host_owned_provider_auth";
const DENY_HOST_WORKLOAD_IDENTITY: &str = "host_owned_workload_identity";
const DENY_LOGIN_NOT_AVAILABLE: &str = "account_slot_login_not_available";
const DENY_SWITCH_NOT_AVAILABLE: &str = "thread_account_switch_not_available";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountSlotsManifest {
    schema_version: u32,
    revision: u64,
    slots: Vec<AccountSlotManifest>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountSlotManifest {
    account_slot_id: String,
    label: String,
    auth_home: PathBuf,
    is_default: bool,
    status: ManifestSlotStatus,
    attempt_generation: u64,
    updated_at: i64,
    error_code: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ManifestSlotStatus {
    LoginRequired,
    Ready,
    Failed,
}

impl From<ManifestSlotStatus> for AccountSlotStatus {
    fn from(status: ManifestSlotStatus) -> Self {
        match status {
            ManifestSlotStatus::LoginRequired => Self::LoginRequired,
            ManifestSlotStatus::Ready => Self::Ready,
            ManifestSlotStatus::Failed => Self::Failed,
        }
    }
}

impl AccountSlotsManifest {
    fn load(path: &Path, process_home: &Path) -> io::Result<Option<Self>> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let manifest: Self = toml::from_str(&contents)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid manifest"))?;
        manifest.validate(process_home)?;
        Ok(Some(manifest))
    }

    fn persist(&self, path: &Path) -> io::Result<()> {
        let contents = toml::to_string(self)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid manifest"))?;
        write_atomically(path, &contents)
    }

    fn validate(&self, process_home: &Path) -> io::Result<()> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION
            || self.revision == 0
            || self.slots.is_empty()
            || self.slots.len() > MAX_MANIFEST_SLOTS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid manifest metadata",
            ));
        }

        let private_homes_root = process_home.join(PRIVATE_HOMES_DIR);
        let mut slot_ids = HashSet::with_capacity(self.slots.len());
        let mut default_count = 0;
        for slot in &self.slots {
            if slot.label.trim().is_empty()
                || slot.label.len() > 128
                || !slot.auth_home.is_absolute()
                || !slot_ids.insert(slot.account_slot_id.as_str())
                || slot
                    .error_code
                    .as_ref()
                    .is_some_and(|code| !valid_manifest_token(code))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid account slot",
                ));
            }
            if slot.is_default {
                default_count += 1;
                if slot.account_slot_id != DEFAULT_SLOT_ID || slot.auth_home != process_home {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid default account home",
                    ));
                }
            } else {
                if !valid_account_slot_id(&slot.account_slot_id) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid account slot id",
                    ));
                }
                validate_private_auth_home(
                    process_home,
                    &private_homes_root,
                    &slot.account_slot_id,
                    &slot.auth_home,
                )?;
            }
        }
        if default_count != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "manifest must contain one default slot",
            ));
        }
        Ok(())
    }
}

fn valid_account_slot_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|uuid| uuid.simple().to_string() == value)
}

fn validate_private_auth_home(
    process_home: &Path,
    private_homes_root: &Path,
    account_slot_id: &str,
    auth_home: &Path,
) -> io::Result<()> {
    let slot_root = private_homes_root.join(account_slot_id);
    if auth_home != slot_root
        && (auth_home.parent() != Some(slot_root.as_path()) || auth_home.file_name().is_none())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid private account home",
        ));
    }

    reject_symlink(private_homes_root)?;
    reject_symlink(&slot_root)?;
    reject_symlink(auth_home)?;

    let canonical_private_root = match private_homes_root.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let canonical_process_home = process_home.canonicalize()?;
    if canonical_private_root.parent() != Some(canonical_process_home.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private account root escapes process home",
        ));
    }

    let canonical_auth_home = match auth_home.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let canonical_slot_root = slot_root.canonicalize()?;
    if canonical_slot_root.parent() != Some(canonical_private_root.as_path())
        || canonical_slot_root.file_name() != Some(std::ffi::OsStr::new(account_slot_id))
        || (canonical_auth_home != canonical_slot_root
            && canonical_auth_home.parent() != Some(canonical_slot_root.as_path()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private account home escapes private root",
        ));
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private account path must not be a symlink",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn valid_manifest_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

#[derive(Clone)]
struct AccountSlotRecord {
    manifest: AccountSlotManifest,
    runtime: Arc<AccountRuntimeCell>,
    binding_transition: Arc<Mutex<()>>,
    active_login_operation_id: Option<String>,
    active_logout_operation_id: Option<String>,
    completed_login_operation_id: Option<String>,
}

pub(crate) struct AccountRuntimeBundle {
    pub(crate) runtime_version: AtomicU64,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: SharedModelsManager,
}

#[derive(Default)]
struct AccountRuntimeCell(StdMutex<Option<Arc<AccountRuntimeBundle>>>);

impl AccountRuntimeCell {
    fn get(&self) -> Option<Arc<AccountRuntimeBundle>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set(&self, runtime: Arc<AccountRuntimeBundle>) -> Result<(), Arc<AccountRuntimeBundle>> {
        let mut current = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.is_some() {
            return Err(runtime);
        }
        *current = Some(runtime);
        Ok(())
    }

    fn replace(&self, runtime: Arc<AccountRuntimeBundle>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(runtime);
    }
}

struct AccountRegistryState {
    revision: u64,
    slots: Vec<AccountSlotRecord>,
    manifest_error: Option<&'static str>,
    manifest_present: bool,
    projection_dirty: bool,
}

pub(crate) struct AccountRegistry {
    config: Arc<Config>,
    auth_config_template: AuthConfig,
    thread_store: Arc<dyn ThreadStore>,
    state: RwLock<AccountRegistryState>,
    mutation_lock: Mutex<()>,
    browser_login: StdMutex<Option<BrowserLoginOwner>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BrowserLoginOwner {
    Default(String),
    Slot(String),
}

impl AccountRegistry {
    pub(crate) fn new(
        config: Arc<Config>,
        default_auth_manager: Arc<AuthManager>,
        default_models_manager: SharedModelsManager,
        thread_store: Arc<dyn ThreadStore>,
    ) -> Self {
        let manifest_path = config.codex_home.join(MANIFEST_FILE);
        let (manifest, manifest_error, manifest_present) =
            match AccountSlotsManifest::load(&manifest_path, &config.codex_home) {
                Ok(Some(manifest)) => (manifest, None, true),
                Ok(None) => (virtual_default_manifest(&config.codex_home), None, false),
                Err(_) => (
                    virtual_default_manifest(&config.codex_home),
                    Some(DENY_MANIFEST_INVALID),
                    false,
                ),
            };
        let default_runtime = Arc::new(AccountRuntimeBundle {
            runtime_version: AtomicU64::new(0),
            auth_manager: default_auth_manager,
            models_manager: default_models_manager,
        });
        let mut slots = manifest
            .slots
            .into_iter()
            .map(|manifest| {
                let runtime = Arc::new(AccountRuntimeCell::default());
                if manifest.is_default {
                    let _ = runtime.set(Arc::clone(&default_runtime));
                }
                AccountSlotRecord {
                    manifest,
                    runtime,
                    binding_transition: Arc::new(Mutex::new(())),
                    active_login_operation_id: None,
                    active_logout_operation_id: None,
                    completed_login_operation_id: None,
                }
            })
            .collect::<Vec<_>>();
        slots.sort_by(|left, right| {
            left.manifest
                .account_slot_id
                .cmp(&right.manifest.account_slot_id)
        });

        Self {
            auth_config_template: config.auth_config(),
            thread_store,
            config,
            state: RwLock::new(AccountRegistryState {
                revision: manifest.revision,
                slots,
                manifest_error,
                manifest_present,
                projection_dirty: false,
            }),
            mutation_lock: Mutex::new(()),
            browser_login: StdMutex::new(None),
        }
    }

    pub(crate) async fn list(
        &self,
        params: AccountSlotListParams,
    ) -> Result<AccountSlotListResponse, JSONRPCErrorError> {
        self.reconcile().await?;
        let (revision, slots, manifest_error) = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            (state.revision, state.slots.clone(), state.manifest_error)
        };
        let capability = self.capability(manifest_error);
        let limit = match params.limit.map(|limit| limit as usize) {
            Some(0) => return Err(invalid_params("accountSlot/list limit must be positive")),
            Some(limit) => limit.min(MAX_LIST_LIMIT),
            None => DEFAULT_LIST_LIMIT,
        };
        let start = cursor_start(params.cursor.as_deref(), revision, &slots)?;
        let end = start.saturating_add(limit).min(slots.len());
        let mut data = Vec::with_capacity(end.saturating_sub(start));
        for slot in &slots[start..end] {
            data.push(self.snapshot(slot, revision, &capability).await);
        }
        let next_cursor = if end < slots.len() {
            Some(
                encode_cursor(AccountSlotCursor {
                    revision,
                    after_slot_id: slots[end - 1].manifest.account_slot_id.clone(),
                })
                .map_err(|_| internal_error("account slot cursor could not be serialized"))?,
            )
        } else {
            None
        };

        Ok(AccountSlotListResponse {
            data,
            next_cursor,
            registry_revision: revision,
            multi_account: capability,
        })
    }

    pub(crate) async fn runtime_capability(
        &self,
    ) -> Result<AccountSlotCapability, JSONRPCErrorError> {
        self.reconcile().await?;
        let manifest_error = self
            .state
            .read()
            .map_err(|_| internal_error("account slot registry is unavailable"))?
            .manifest_error;
        Ok(self.capability(manifest_error))
    }

    pub(crate) async fn lock_slot_binding_transition(
        &self,
        account_slot_id: &str,
    ) -> Result<OwnedMutexGuard<()>, JSONRPCErrorError> {
        let binding_transition = {
            let state = self
                .state
                .read()
                .map_err(|_| internal_error("account slot registry is unavailable"))?;
            Arc::clone(
                &state
                    .slots
                    .iter()
                    .find(|slot| slot.manifest.account_slot_id == account_slot_id)
                    .ok_or_else(|| invalid_request("account slot is unavailable"))?
                    .binding_transition,
            )
        };
        Ok(binding_transition.lock_owned().await)
    }

    async fn snapshot(
        &self,
        slot: &AccountSlotRecord,
        revision: u64,
        capability: &AccountSlotCapability,
    ) -> AccountSlotSnapshot {
        let runtime = if capability.available || slot.manifest.is_default {
            Some(self.runtime(slot).await)
        } else {
            None
        };
        let auth_mode = runtime
            .as_ref()
            .and_then(|runtime| runtime.auth_manager.auth_mode())
            .map(auth_mode_to_api);
        let status = match (slot.manifest.is_default, auth_mode) {
            (true, Some(_)) => AccountSlotStatus::Ready,
            _ => slot.manifest.status.into(),
        };
        let error_code = (status == AccountSlotStatus::Failed)
            .then(|| slot.manifest.error_code.clone())
            .flatten();

        AccountSlotSnapshot {
            account_slot_id: slot.manifest.account_slot_id.clone(),
            label: slot.manifest.label.clone(),
            is_default: slot.manifest.is_default,
            status,
            auth_mode,
            attempt_generation: slot.manifest.attempt_generation,
            registry_revision: revision,
            active_login_operation_id: slot.active_login_operation_id.clone(),
            error_code,
            actions: live_registration::available_actions(
                status,
                capability,
                slot.manifest.is_default,
                slot.active_login_operation_id.is_some()
                    || slot.active_logout_operation_id.is_some(),
            ),
            updated_at: slot.manifest.updated_at,
        }
    }

    async fn runtime(&self, slot: &AccountSlotRecord) -> Arc<AccountRuntimeBundle> {
        if let Some(runtime) = slot.runtime.get() {
            return runtime;
        }
        let runtime_version = match self
            .thread_store
            .execution_account_slot_runtime_state(slot.manifest.account_slot_id.clone())
            .await
        {
            Ok((runtime_version, _)) => runtime_version,
            Err(error) => {
                tracing::warn!(
                    account_slot_id = %slot.manifest.account_slot_id,
                    "failed to read account runtime version; using manifest recovery projection: {error}"
                );
                0
            }
        };
        let auth_home = if runtime_version == 0 || slot.manifest.is_default {
            slot.manifest.auth_home.clone()
        } else {
            self.runtime_home(&slot.manifest.account_slot_id, runtime_version)
        };
        let runtime = self.build_runtime(auth_home, runtime_version).await;
        let _ = slot.runtime.set(Arc::clone(&runtime));
        slot.runtime.get().unwrap_or(runtime)
    }

    async fn build_runtime(
        &self,
        auth_home: PathBuf,
        runtime_version: u64,
    ) -> Arc<AccountRuntimeBundle> {
        let mut auth_config = self.auth_config_template.clone();
        auth_config.codex_home = auth_home.clone();
        let auth_manager = AuthManager::shared_from_managed_auth_config(auth_config).await;
        let provider = create_model_provider(
            self.config.model_provider.clone(),
            Some(Arc::clone(&auth_manager)),
        );
        let models_manager =
            provider.models_manager(auth_home.clone(), self.config.model_catalog.clone());
        Arc::new(AccountRuntimeBundle {
            runtime_version: AtomicU64::new(runtime_version),
            auth_manager,
            models_manager,
        })
    }

    pub(crate) fn runtime_home(&self, slot_id: &str, runtime_version: u64) -> PathBuf {
        self.config
            .codex_home
            .join(PRIVATE_HOMES_DIR)
            .join(slot_id)
            .join(format!("runtime-{runtime_version}"))
            .to_path_buf()
    }

    fn capability(&self, manifest_error: Option<&'static str>) -> AccountSlotCapability {
        let deny_reason = match self.default_auth_source() {
            AuthSourceKind::CodexAccessTokenEnvironment => Some(DENY_HOST_ACCESS_TOKEN),
            AuthSourceKind::WorkloadIdentity => Some(DENY_HOST_WORKLOAD_IDENTITY),
            AuthSourceKind::External => Some(DENY_HOST_EXTERNAL_AUTH),
            AuthSourceKind::ManagedStore | AuthSourceKind::CodexApiKeyEnvironment => manifest_error,
        }
        .or_else(|| {
            self.provider_has_host_owned_auth()
                .then_some(DENY_HOST_PROVIDER_AUTH)
        });
        AccountSlotCapability {
            available: deny_reason.is_none(),
            deny_reason: deny_reason.map(str::to_string),
        }
    }

    fn default_auth_source(&self) -> AuthSourceKind {
        self.state
            .read()
            .ok()
            .and_then(|state| {
                state
                    .slots
                    .iter()
                    .find(|slot| slot.manifest.is_default)
                    .cloned()
            })
            .and_then(|slot| slot.runtime.get())
            .map(|runtime| runtime.auth_manager.auth_source_kind())
            .unwrap_or(AuthSourceKind::External)
    }

    fn provider_has_host_owned_auth(&self) -> bool {
        let provider = &self.config.model_provider;
        provider.auth.is_some()
            || provider.aws.is_some()
            || provider.experimental_bearer_token.is_some()
            || provider
                .env_key
                .as_ref()
                .is_some_and(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
            || provider.env_http_headers.as_ref().is_some_and(|headers| {
                headers
                    .values()
                    .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
            })
    }

    async fn resolve_execution_account(
        &self,
        binding: ExecutionAccountBinding,
    ) -> Result<(Arc<ExecutionAccountContext>, OwnedMutexGuard<()>), CodexErr> {
        self.reconcile().await.map_err(|error| {
            CodexErr::Fatal(format!(
                "account slot reconciliation failed: {}",
                error.message
            ))
        })?;
        let binding_transition = {
            let state = self
                .state
                .read()
                .map_err(|_| CodexErr::Fatal("account slot registry is unavailable".to_string()))?;
            let slot = state
                .slots
                .iter()
                .find(|slot| slot.manifest.account_slot_id == binding.slot_id)
                .ok_or_else(|| {
                    CodexErr::InvalidRequest(format!(
                        "execution account slot `{}` is unavailable",
                        binding.slot_id
                    ))
                })?;
            if slot.active_login_operation_id.is_some() || slot.active_logout_operation_id.is_some()
            {
                return Err(CodexErr::InvalidRequest(format!(
                    "execution account slot `{}` is changing credentials",
                    binding.slot_id
                )));
            }
            Arc::clone(&slot.binding_transition)
        };
        let binding_transition = binding_transition.lock_owned().await;
        let (slot, manifest_error) = {
            let state = self
                .state
                .read()
                .map_err(|_| CodexErr::Fatal("account slot registry is unavailable".to_string()))?;
            (
                state
                    .slots
                    .iter()
                    .find(|slot| slot.manifest.account_slot_id == binding.slot_id)
                    .cloned(),
                state.manifest_error,
            )
        };
        let slot = slot.ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "execution account slot `{}` is unavailable",
                binding.slot_id
            ))
        })?;
        let capability = self.capability(manifest_error);
        if !slot.manifest.is_default && !capability.available {
            return Err(CodexErr::InvalidRequest(format!(
                "execution account slot `{}` is unavailable: {}",
                binding.slot_id,
                capability
                    .deny_reason
                    .as_deref()
                    .unwrap_or("multi_account_unavailable")
            )));
        }
        if !slot.manifest.is_default && slot.manifest.status != ManifestSlotStatus::Ready {
            return Err(CodexErr::InvalidRequest(format!(
                "execution account slot `{}` is not ready",
                binding.slot_id
            )));
        }
        if slot.active_login_operation_id.is_some() || slot.active_logout_operation_id.is_some() {
            return Err(CodexErr::InvalidRequest(format!(
                "execution account slot `{}` is changing credentials",
                binding.slot_id
            )));
        }
        let runtime = self.runtime(&slot).await;
        if !slot.manifest.is_default && runtime.auth_manager.auth().await.is_none() {
            return Err(CodexErr::InvalidRequest(format!(
                "execution account slot `{}` is not ready",
                binding.slot_id
            )));
        }
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

pub(crate) mod live_registration;

impl ExecutionAccountResolver for AccountRegistry {
    fn resolve(&self, binding: ExecutionAccountBinding) -> ExecutionAccountResolverFuture<'_> {
        Box::pin(async move {
            let (execution_account, _binding_transition) =
                self.resolve_execution_account(binding).await?;
            Ok(execution_account)
        })
    }

    fn resolve_for_transition(
        &self,
        binding: ExecutionAccountBinding,
    ) -> ExecutionAccountTransitionResolverFuture<'_> {
        Box::pin(async move {
            let (execution_account, binding_transition) =
                self.resolve_execution_account(binding).await?;
            Ok(ResolvedExecutionAccountTransition::with_readiness_lease(
                execution_account,
                binding_transition,
            ))
        })
    }
}

fn virtual_default_manifest(process_home: &Path) -> AccountSlotsManifest {
    AccountSlotsManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        revision: 1,
        slots: vec![AccountSlotManifest {
            account_slot_id: DEFAULT_SLOT_ID.to_string(),
            label: "Default account".to_string(),
            auth_home: process_home.to_path_buf(),
            is_default: true,
            status: ManifestSlotStatus::LoginRequired,
            attempt_generation: 0,
            updated_at: 0,
            error_code: None,
        }],
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountSlotCursor {
    revision: u64,
    after_slot_id: String,
}

#[derive(Debug, Eq, PartialEq)]
enum CursorError {
    Invalid,
    Stale,
}

fn encode_cursor(cursor: AccountSlotCursor) -> Result<String, serde_json::Error> {
    serde_json::to_vec(&cursor).map(|json| URL_SAFE_NO_PAD.encode(json))
}

fn decode_cursor(cursor: &str, revision: u64) -> Result<AccountSlotCursor, CursorError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| CursorError::Invalid)?;
    let cursor: AccountSlotCursor =
        serde_json::from_slice(&bytes).map_err(|_| CursorError::Invalid)?;
    if cursor.revision != revision {
        return Err(CursorError::Stale);
    }
    Ok(cursor)
}

fn cursor_start(
    cursor: Option<&str>,
    revision: u64,
    slots: &[AccountSlotRecord],
) -> Result<usize, JSONRPCErrorError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let cursor = match decode_cursor(cursor, revision) {
        Ok(cursor) => cursor,
        Err(CursorError::Invalid) => {
            return Err(invalid_params("accountSlot/list cursor is invalid"));
        }
        Err(CursorError::Stale) => {
            return Err(invalid_params(
                "accountSlot/list cursor is stale; restart pagination",
            ));
        }
    };
    slots
        .iter()
        .position(|slot| slot.manifest.account_slot_id == cursor.after_slot_id)
        .map(|index| index + 1)
        .ok_or_else(|| invalid_params("accountSlot/list cursor is invalid"))
}

#[cfg(test)]
#[path = "account_registry_tests.rs"]
mod tests;
