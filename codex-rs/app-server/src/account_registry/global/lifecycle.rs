use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_transport::ManagedAccountCatalog;
use codex_app_server_transport::ManagedAccountId;
use codex_login::auth::ManagedAuthStaging;
use codex_login::auth::ManagedAuthState;
use codex_login::auth::managed_auth_state;
use codex_login::auth::remove_managed_auth;

use super::AccountId;
use super::CatalogError;
use super::TokenManagerClient;
use super::token_manager::TokenManagerLifecycle;
use crate::account_registry::AccountRegistry;
use crate::error_code::invalid_request;

const ACCOUNT_CATALOG_RELATIVE_PATH: &str = ".config/codex-accounts.tsv";
const LIFECYCLE_UNAVAILABLE: &str = "managed account lifecycle is unavailable";
const LIFECYCLE_REJECTED: &str = "managed account lifecycle was rejected";
const LIFECYCLE_NOT_READY: &str = "managed account credential committed but is not ready";

/// Begins one exact-account writer gate and returns its connection-owned session.
trait LifecycleAuthority {
    type Session: LifecycleSession;

    fn begin(
        &self,
        account_id: AccountId,
    ) -> impl std::future::Future<Output = Result<Self::Session, CatalogError>> + Send;
}

/// Finishes one connection-owned writer gate after the credential decision.
pub(crate) trait LifecycleSession: Send {
    fn commit(self) -> impl std::future::Future<Output = Result<(), CatalogError>> + Send;

    fn abort(self) -> impl std::future::Future<Output = ()> + Send;
}

impl LifecycleAuthority for TokenManagerClient {
    type Session = TokenManagerLifecycle;

    async fn begin(&self, account_id: AccountId) -> Result<Self::Session, CatalogError> {
        self.begin_lifecycle(account_id).await
    }
}

impl LifecycleSession for TokenManagerLifecycle {
    async fn commit(self) -> Result<(), CatalogError> {
        TokenManagerLifecycle::commit(self).await
    }

    async fn abort(self) {
        TokenManagerLifecycle::abort(self).await;
    }
}

pub(crate) struct GlobalLoginLifecycle<S = TokenManagerLifecycle> {
    account_id: AccountId,
    auth_home: PathBuf,
    expected: Option<ManagedAuthState>,
    staging: ManagedAuthStaging,
    authority: S,
}

impl<S> GlobalLoginLifecycle<S> {
    pub(crate) fn staging_home(&self) -> &Path {
        self.staging.home()
    }
}

impl AccountRegistry {
    pub(crate) fn global_managed_mode(&self) -> Result<bool, JSONRPCErrorError> {
        let Some(owner_home) = self.global_directory_user_home.as_deref() else {
            return Ok(false);
        };
        match std::fs::symlink_metadata(owner_home.join(ACCOUNT_CATALOG_RELATIVE_PATH)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Ok(_) => {}
            Err(_) => return Err(lifecycle_unavailable()),
        }
        let catalog = ManagedAccountCatalog::load_from_owner_home(owner_home)
            .map_err(|_| lifecycle_rejected())?;
        catalog
            .account_for_home(&self.config.codex_home)
            .ok_or_else(lifecycle_rejected)?;
        Ok(true)
    }

    pub(crate) async fn begin_global_login(
        &self,
        params: &AccountSlotLoginStartParams,
    ) -> Result<GlobalLoginLifecycle, JSONRPCErrorError> {
        let authority = self
            .token_manager_client
            .as_ref()
            .ok_or_else(lifecycle_unavailable)?;
        self.begin_global_login_with(params, authority).await
    }

    async fn begin_global_login_with<A: LifecycleAuthority>(
        &self,
        params: &AccountSlotLoginStartParams,
        authority: &A,
    ) -> Result<GlobalLoginLifecycle<A::Session>, JSONRPCErrorError> {
        let account_id = exact_chatgpt_account(params)?;
        let auth_home = self.global_account_home(account_id)?;
        let authority = authority
            .begin(account_id)
            .await
            .map_err(|_| lifecycle_unavailable())?;
        let expected = match managed_auth_state(&auth_home) {
            Ok(expected) => expected,
            Err(_) => {
                authority.abort().await;
                return Err(lifecycle_rejected());
            }
        };
        if let Some(expected) = expected.as_ref()
            && self
                .established_identity_matches(account_id, &auth_home, expected)
                .await
                .is_err()
        {
            authority.abort().await;
            return Err(lifecycle_rejected());
        }
        let staging = match ManagedAuthStaging::create(&auth_home) {
            Ok(staging) => staging,
            Err(_) => {
                authority.abort().await;
                return Err(lifecycle_unavailable());
            }
        };
        Ok(GlobalLoginLifecycle {
            account_id,
            auth_home,
            expected,
            staging,
            authority,
        })
    }

    pub(crate) async fn commit_global_login<S: LifecycleSession>(
        &self,
        lifecycle: GlobalLoginLifecycle<S>,
    ) -> Result<ManagedAuthState, JSONRPCErrorError> {
        let GlobalLoginLifecycle {
            account_id,
            auth_home,
            expected,
            staging,
            authority,
        } = lifecycle;
        let promotion = {
            let _identity_fence = self.mutation_lock.lock().await;
            (|| {
                self.validate_global_account_home(account_id, &auth_home)?;
                let candidate = staging
                    .candidate_state()
                    .map_err(|_| lifecycle_rejected())?;
                self.identity_is_unique(account_id, candidate.account_id())?;
                staging
                    .promote(expected.as_ref())
                    .map_err(|_| lifecycle_rejected())
            })()
        };
        let promotion = match promotion {
            Ok(promotion) => promotion,
            Err(_) => {
                authority.abort().await;
                return Err(lifecycle_rejected());
            }
        };
        let promoted = promotion.accept();
        self.global_runtimes.lock().await.remove(&account_id);
        if authority.commit().await.is_err() {
            return Err(lifecycle_unavailable());
        }
        self.observe_committed_identity(account_id, &auth_home, &promoted)
            .await
            .map_err(|_| lifecycle_not_ready())?;
        Ok(promoted)
    }

    pub(crate) async fn abort_global_login<S: LifecycleSession>(
        &self,
        lifecycle: GlobalLoginLifecycle<S>,
    ) {
        lifecycle.authority.abort().await;
    }

    pub(crate) async fn logout_global_account(
        &self,
        params: &AccountSlotLogoutParams,
    ) -> Result<(), JSONRPCErrorError> {
        if AccountId::parse(&params.account_slot_id).is_some_and(|account_id| {
            self.global_active_logins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&account_id)
        }) {
            return Err(invalid_request("managed account login is active"));
        }
        let authority = self
            .token_manager_client
            .as_ref()
            .ok_or_else(lifecycle_unavailable)?;
        self.logout_global_account_with(params, authority).await
    }

    async fn logout_global_account_with<A: LifecycleAuthority>(
        &self,
        params: &AccountSlotLogoutParams,
        authority: &A,
    ) -> Result<(), JSONRPCErrorError> {
        let account_id = AccountId::parse(&params.account_slot_id)
            .ok_or_else(|| invalid_request("managed account is invalid"))?;
        let auth_home = self.global_account_home(account_id)?;
        let (snapshots, update) = self
            .global_inventory_snapshot(
                &self.refresh_global_directory(),
                chrono::Utc::now().timestamp(),
            )
            .await;
        let attempt_generation = snapshots
            .iter()
            .find(|snapshot| snapshot.account_slot_id == params.account_slot_id)
            .map(|snapshot| snapshot.attempt_generation)
            .ok_or_else(|| invalid_request("managed account is not registered"))?;
        if update.revision != params.expected_registry_revision
            || attempt_generation != params.expected_attempt_generation
        {
            return Err(invalid_request("managed account revision is stale"));
        }
        let authority = authority
            .begin(account_id)
            .await
            .map_err(|_| lifecycle_unavailable())?;
        let expected = match managed_auth_state(&auth_home) {
            Ok(Some(expected)) => expected,
            Ok(None) | Err(_) => {
                authority.abort().await;
                return Err(lifecycle_rejected());
            }
        };
        if self
            .established_identity_matches(account_id, &auth_home, &expected)
            .await
            .is_err()
            || self
                .validate_global_account_home(account_id, &auth_home)
                .is_err()
        {
            authority.abort().await;
            return Err(lifecycle_rejected());
        }
        let removal = match remove_managed_auth(&auth_home, &expected) {
            Ok(removal) => removal,
            Err(_) => {
                authority.abort().await;
                return Err(lifecycle_rejected());
            }
        };
        removal.accept();
        self.global_runtimes.lock().await.remove(&account_id);
        if authority.commit().await.is_err() {
            return Err(lifecycle_unavailable());
        }
        self.observe_committed_logout(account_id, &auth_home, &expected)
            .await
            .map_err(|_| lifecycle_not_ready())?;
        Ok(())
    }

    fn global_account_home(&self, account_id: AccountId) -> Result<PathBuf, JSONRPCErrorError> {
        let catalog = self.managed_account_catalog()?;
        let managed_id =
            ManagedAccountId::parse(&account_id.to_string()).ok_or_else(lifecycle_rejected)?;
        catalog
            .home(managed_id)
            .map(Path::to_path_buf)
            .ok_or_else(|| invalid_request("managed account is not registered"))
    }

    fn managed_account_catalog(&self) -> Result<ManagedAccountCatalog, JSONRPCErrorError> {
        let owner_home = self
            .global_directory_user_home
            .as_deref()
            .ok_or_else(lifecycle_unavailable)?;
        let catalog = ManagedAccountCatalog::load_from_owner_home(owner_home)
            .map_err(|_| lifecycle_rejected())?;
        catalog
            .account_for_home(&self.config.codex_home)
            .ok_or_else(lifecycle_rejected)?;
        Ok(catalog)
    }

    fn validate_global_account_home(
        &self,
        account_id: AccountId,
        expected_home: &Path,
    ) -> Result<(), JSONRPCErrorError> {
        (self.global_account_home(account_id)?.as_path() == expected_home)
            .then_some(())
            .ok_or_else(lifecycle_rejected)
    }

    fn identity_is_unique(
        &self,
        account_id: AccountId,
        candidate_identity: &str,
    ) -> Result<(), JSONRPCErrorError> {
        let catalog = self.managed_account_catalog()?;
        for (other_id, home) in catalog.entries() {
            if other_id.number() == account_id.number() {
                continue;
            }
            match managed_auth_state(home) {
                Ok(Some(state)) if state.account_id() == candidate_identity => {
                    return Err(lifecycle_rejected());
                }
                Ok(_) => {}
                Err(_) => return Err(lifecycle_rejected()),
            }
        }
        Ok(())
    }

    async fn established_identity_matches(
        &self,
        account_id: AccountId,
        auth_home: &Path,
        state: &ManagedAuthState,
    ) -> Result<(), JSONRPCErrorError> {
        self.ensure_global_catalog()
            .await
            .map_err(|_| lifecycle_unavailable())?;
        let source_ref = super::directory::subscription_source_ref(state.account_id(), auth_home)
            .ok_or_else(lifecycle_rejected)?;
        self.global_catalog
            .projection_for(account_id, &source_ref, chrono::Utc::now().timestamp())
            .ok_or_else(lifecycle_rejected)
            .map(|_| ())
    }

    async fn observe_committed_identity(
        &self,
        account_id: AccountId,
        auth_home: &Path,
        state: &ManagedAuthState,
    ) -> Result<(), JSONRPCErrorError> {
        let client = self
            .token_manager_client
            .as_ref()
            .ok_or_else(lifecycle_unavailable)?;
        self.refresh_global_catalog(client)
            .await
            .map_err(|_| lifecycle_unavailable())?;
        if managed_auth_state(auth_home).ok().flatten().as_ref() != Some(state) {
            return Err(lifecycle_rejected());
        }
        let source_ref = super::directory::subscription_source_ref(state.account_id(), auth_home)
            .ok_or_else(lifecycle_rejected)?;
        self.global_catalog
            .projection_for(account_id, &source_ref, chrono::Utc::now().timestamp())
            .ok_or_else(lifecycle_rejected)
            .map(|_| ())
    }

    async fn observe_committed_logout(
        &self,
        account_id: AccountId,
        auth_home: &Path,
        previous: &ManagedAuthState,
    ) -> Result<(), JSONRPCErrorError> {
        let client = self
            .token_manager_client
            .as_ref()
            .ok_or_else(lifecycle_unavailable)?;
        self.refresh_global_catalog(client)
            .await
            .map_err(|_| lifecycle_unavailable())?;
        if managed_auth_state(auth_home).ok().flatten().is_some() {
            return Err(lifecycle_rejected());
        }
        let source_ref =
            super::directory::subscription_source_ref(previous.account_id(), auth_home)
                .ok_or_else(lifecycle_rejected)?;
        if self
            .global_catalog
            .projection_for(account_id, &source_ref, chrono::Utc::now().timestamp())
            .is_some()
        {
            return Err(lifecycle_rejected());
        }
        Ok(())
    }
}

fn exact_chatgpt_account(
    params: &AccountSlotLoginStartParams,
) -> Result<AccountId, JSONRPCErrorError> {
    let slot_id = match params {
        AccountSlotLoginStartParams::Chatgpt { slot_id, .. } => slot_id.as_deref(),
        AccountSlotLoginStartParams::ApiKey { .. }
        | AccountSlotLoginStartParams::ChatgptDeviceCode { .. }
        | AccountSlotLoginStartParams::ChatgptAuthTokens { .. } => {
            return Err(invalid_request(
                "managed accounts require browser ChatGPT login",
            ));
        }
    };
    slot_id
        .and_then(AccountId::parse)
        .ok_or_else(|| invalid_request("managed account must be an exact registered C slot"))
}

fn lifecycle_unavailable() -> JSONRPCErrorError {
    invalid_request(LIFECYCLE_UNAVAILABLE)
}

fn lifecycle_rejected() -> JSONRPCErrorError {
    invalid_request(LIFECYCLE_REJECTED)
}

fn lifecycle_not_ready() -> JSONRPCErrorError {
    invalid_request(LIFECYCLE_NOT_READY)
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
