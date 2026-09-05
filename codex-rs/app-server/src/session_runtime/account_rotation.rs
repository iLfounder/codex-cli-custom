use std::collections::HashMap;
use std::collections::HashSet;

use codex_app_server_protocol::AccountRotationChangedNotification;
use codex_app_server_protocol::AccountRotationReadResponse;
use codex_app_server_protocol::AccountRotationSnapshot;
use codex_app_server_protocol::AccountRotationUpdateParams;
use codex_app_server_protocol::AccountRotationUpdateResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadAccountRotationMode as ApiRotationMode;
use codex_app_server_protocol::ThreadAccountRotationReadParams;
use codex_app_server_protocol::ThreadAccountRotationReadResponse;
use codex_app_server_protocol::ThreadAccountRotationResetParams;
use codex_app_server_protocol::ThreadAccountRotationResetResponse;
use codex_app_server_protocol::ThreadAccountRotationSnapshot;
use codex_app_server_protocol::ThreadAccountRotationSource;
use codex_app_server_protocol::ThreadAccountRotationUpdateParams;
use codex_app_server_protocol::ThreadAccountRotationUpdateResponse;
use codex_protocol::ThreadId;
use codex_thread_store::AccountRotationProfile;
use codex_thread_store::AccountRotationProfileUpdate;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ThreadAccountRotationMode;
use codex_thread_store::ThreadAccountRotationPolicy;
use codex_thread_store::ThreadAccountRotationPolicyRevision;
use codex_thread_store::ThreadStoreError;

use super::SessionRuntimeEngine;
use crate::account_registry::RotationSlotIdentity;
use crate::account_registry::live_registration::structured_invalid_request;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;

const ERROR_STALE_REVISION: &str = "staleRotationRevision";
const ERROR_MEMBER_MISSING: &str = "accountRotationMemberMissing";

impl SessionRuntimeEngine {
    pub(crate) async fn read_global_account_rotation(
        &self,
    ) -> Result<AccountRotationReadResponse, JSONRPCErrorError> {
        let rotation = self
            .thread_store
            .account_rotation_global_profile()
            .await
            .map_err(store_error)?
            .map(api_profile_snapshot);
        Ok(AccountRotationReadResponse { rotation })
    }

    pub(crate) async fn update_global_account_rotation(
        &self,
        params: AccountRotationUpdateParams,
    ) -> Result<AccountRotationUpdateResponse, JSONRPCErrorError> {
        let update = self
            .validated_update(
                params.mode,
                params.fixed_account_slot_id,
                params.automatic_account_slot_ids,
            )
            .await?;
        let Some(profile) = self
            .thread_store
            .compare_and_swap_account_rotation_global_profile(
                params.expected_rotation_revision,
                update,
            )
            .await
            .map_err(store_error)?
        else {
            return Err(stale_revision());
        };
        let rotation = api_profile_snapshot(profile);
        self.mark_dirty();
        self.outgoing
            .send_server_notification(ServerNotification::AccountRotationChanged(
                AccountRotationChangedNotification {
                    rotation: rotation.clone(),
                },
            ))
            .await;
        Ok(AccountRotationUpdateResponse { rotation })
    }

    pub(crate) async fn read_account_rotation(
        &self,
        params: ThreadAccountRotationReadParams,
    ) -> Result<ThreadAccountRotationReadResponse, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        self.ensure_account_rotation_thread_exists(thread_id)
            .await?;
        Ok(ThreadAccountRotationReadResponse {
            rotation: self.thread_account_rotation_snapshot(thread_id).await?,
        })
    }

    pub(crate) async fn update_account_rotation(
        &self,
        params: ThreadAccountRotationUpdateParams,
    ) -> Result<ThreadAccountRotationUpdateResponse, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        self.ensure_account_rotation_thread_exists(thread_id)
            .await?;
        let update = self
            .validated_update(
                params.mode,
                params.fixed_account_slot_id,
                params.automatic_account_slot_ids,
            )
            .await?;
        let Some(profile) = self
            .thread_store
            .compare_and_swap_thread_account_rotation_override(
                thread_id,
                params.expected_rotation_revision,
                update,
            )
            .await
            .map_err(store_error)?
        else {
            return Err(stale_revision());
        };
        let cursor = self
            .thread_store
            .thread_account_rotation_policy(thread_id)
            .await
            .map_err(store_error)?
            .last_committed_account_slot_id;
        let global_profile_revision = self
            .thread_store
            .account_rotation_global_profile()
            .await
            .map_err(store_error)?
            .map(|profile| profile.revision);
        let rotation = api_thread_snapshot(
            ThreadAccountRotationPolicy {
                mode: profile.mode,
                fixed_account_slot_id: profile.fixed_account_slot_id,
                automatic_account_slot_ids: profile.automatic_account_slot_ids,
                revision: ThreadAccountRotationPolicyRevision::Override(profile.revision),
                last_committed_account_slot_id: cursor,
            },
            global_profile_revision,
        );
        self.publish_thread(thread_id).await;
        Ok(ThreadAccountRotationUpdateResponse { rotation })
    }

    pub(crate) async fn reset_account_rotation(
        &self,
        params: ThreadAccountRotationResetParams,
    ) -> Result<ThreadAccountRotationResetResponse, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        self.ensure_account_rotation_thread_exists(thread_id)
            .await?;
        if !self
            .thread_store
            .reset_thread_account_rotation_override(thread_id, params.expected_rotation_revision)
            .await
            .map_err(store_error)?
        {
            return Err(stale_revision());
        }
        let rotation = self.thread_account_rotation_snapshot(thread_id).await?;
        self.publish_thread(thread_id).await;
        Ok(ThreadAccountRotationResetResponse { rotation })
    }

    pub(super) async fn account_rotation_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> Option<ThreadAccountRotationSnapshot> {
        self.thread_account_rotation_snapshot(thread_id).await.ok()
    }

    async fn thread_account_rotation_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> Result<ThreadAccountRotationSnapshot, JSONRPCErrorError> {
        let policy = self
            .thread_store
            .thread_account_rotation_policy(thread_id)
            .await
            .map_err(store_error)?;
        let global_profile_revision = match policy.revision {
            ThreadAccountRotationPolicyRevision::Inherit(0) => None,
            ThreadAccountRotationPolicyRevision::Inherit(revision) => Some(revision),
            ThreadAccountRotationPolicyRevision::Override(_) => self
                .thread_store
                .account_rotation_global_profile()
                .await
                .map_err(store_error)?
                .map(|profile| profile.revision),
        };
        Ok(api_thread_snapshot(policy, global_profile_revision))
    }

    async fn validated_update(
        &self,
        mode: ApiRotationMode,
        fixed_account_slot_id: Option<String>,
        automatic_account_slot_ids: Vec<String>,
    ) -> Result<AccountRotationProfileUpdate, JSONRPCErrorError> {
        let slots = self.account_registry.rotation_slot_inventory().await?;
        validate_update(
            mode,
            fixed_account_slot_id,
            automatic_account_slot_ids,
            &slots,
        )
    }

    async fn ensure_account_rotation_thread_exists(
        &self,
        thread_id: ThreadId,
    ) -> Result<(), JSONRPCErrorError> {
        if self.thread_manager.get_thread(thread_id).await.is_ok() {
            return Ok(());
        }
        match self
            .thread_store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
        {
            Ok(_) => Ok(()),
            Err(ThreadStoreError::ThreadNotFound { .. }) => Err(
                crate::error_code::invalid_request(format!("thread not found: {thread_id}")),
            ),
            Err(error) => Err(internal_error(format!(
                "failed to read account rotation thread: {error}"
            ))),
        }
    }
}

fn validate_update(
    mode: ApiRotationMode,
    fixed_account_slot_id: Option<String>,
    mut automatic_account_slot_ids: Vec<String>,
    slots: &[RotationSlotIdentity],
) -> Result<AccountRotationProfileUpdate, JSONRPCErrorError> {
    let by_id = slots
        .iter()
        .map(|slot| (slot.account_slot_id.as_str(), slot.account_number))
        .collect::<HashMap<_, _>>();
    if fixed_account_slot_id
        .as_deref()
        .is_some_and(|slot_id| !by_id.contains_key(slot_id))
    {
        return Err(member_missing());
    }
    let mut seen = HashSet::with_capacity(automatic_account_slot_ids.len());
    if automatic_account_slot_ids
        .iter()
        .any(|slot_id| slot_id.is_empty() || !seen.insert(slot_id.as_str()))
    {
        return Err(invalid_params(
            "automatic account rotation slots must be non-empty and distinct",
        ));
    }
    if automatic_account_slot_ids
        .iter()
        .any(|slot_id| !by_id.contains_key(slot_id.as_str()))
    {
        return Err(member_missing());
    }
    if mode == ApiRotationMode::Fixed && fixed_account_slot_id.is_none() {
        return Err(invalid_params(
            "fixed account rotation requires a fixed account slot",
        ));
    }
    if mode != ApiRotationMode::Fixed && automatic_account_slot_ids.is_empty() {
        return Err(invalid_params(
            "automatic account rotation requires at least one account slot",
        ));
    }
    automatic_account_slot_ids.sort_by_key(|slot_id| by_id[slot_id.as_str()]);
    Ok(AccountRotationProfileUpdate {
        mode: store_mode(mode),
        fixed_account_slot_id,
        automatic_account_slot_ids,
    })
}

fn parse_thread_id(thread_id: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(thread_id)
        .map_err(|_| invalid_params("thread account rotation threadId is invalid"))
}

fn store_error(error: ThreadStoreError) -> JSONRPCErrorError {
    internal_error(format!("account rotation store failed: {error}"))
}

fn stale_revision() -> JSONRPCErrorError {
    structured_invalid_request(ERROR_STALE_REVISION, "account rotation revision is stale")
}

fn member_missing() -> JSONRPCErrorError {
    structured_invalid_request(
        ERROR_MEMBER_MISSING,
        "account rotation member is unavailable",
    )
}

fn api_profile_snapshot(profile: AccountRotationProfile) -> AccountRotationSnapshot {
    AccountRotationSnapshot {
        mode: api_mode(profile.mode),
        fixed_account_slot_id: profile.fixed_account_slot_id,
        automatic_account_slot_ids: profile.automatic_account_slot_ids,
        revision: profile.revision,
    }
}

fn api_thread_snapshot(
    policy: ThreadAccountRotationPolicy,
    global_profile_revision: Option<u64>,
) -> ThreadAccountRotationSnapshot {
    let (revision, source) = match policy.revision {
        ThreadAccountRotationPolicyRevision::Inherit(0) => {
            (0, ThreadAccountRotationSource::LegacyFixed)
        }
        ThreadAccountRotationPolicyRevision::Inherit(_) => (0, ThreadAccountRotationSource::Global),
        ThreadAccountRotationPolicyRevision::Override(revision) => {
            (revision, ThreadAccountRotationSource::Override)
        }
    };
    ThreadAccountRotationSnapshot {
        mode: api_mode(policy.mode),
        fixed_account_slot_id: policy.fixed_account_slot_id,
        automatic_account_slot_ids: policy.automatic_account_slot_ids,
        revision,
        last_committed_account_slot_id: policy.last_committed_account_slot_id,
        source,
        global_profile_revision,
    }
}

fn api_mode(mode: ThreadAccountRotationMode) -> ApiRotationMode {
    match mode {
        ThreadAccountRotationMode::Fixed => ApiRotationMode::Fixed,
        ThreadAccountRotationMode::QuotaAware => ApiRotationMode::QuotaAware,
        ThreadAccountRotationMode::RoundRobin => ApiRotationMode::RoundRobin,
        ThreadAccountRotationMode::ExhaustThenNext => ApiRotationMode::ExhaustThenNext,
    }
}

fn store_mode(mode: ApiRotationMode) -> ThreadAccountRotationMode {
    match mode {
        ApiRotationMode::Fixed => ThreadAccountRotationMode::Fixed,
        ApiRotationMode::QuotaAware => ThreadAccountRotationMode::QuotaAware,
        ApiRotationMode::RoundRobin => ThreadAccountRotationMode::RoundRobin,
        ApiRotationMode::ExhaustThenNext => ThreadAccountRotationMode::ExhaustThenNext,
    }
}

#[cfg(test)]
#[path = "account_rotation_tests.rs"]
mod tests;
