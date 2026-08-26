use std::collections::HashMap;
use std::collections::HashSet;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ThreadAccountRotationMode as ApiRotationMode;
use codex_app_server_protocol::ThreadAccountRotationReadParams;
use codex_app_server_protocol::ThreadAccountRotationReadResponse;
use codex_app_server_protocol::ThreadAccountRotationSnapshot;
use codex_app_server_protocol::ThreadAccountRotationUpdateParams;
use codex_app_server_protocol::ThreadAccountRotationUpdateResponse;
use codex_protocol::ThreadId;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ThreadAccountRotationMode;
use codex_thread_store::ThreadAccountRotationPolicy;
use codex_thread_store::ThreadAccountRotationPolicyUpdate;
use codex_thread_store::ThreadStoreError;

use super::SessionRuntimeEngine;
use crate::account_registry::live_registration::structured_invalid_request;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;

const ERROR_STALE_REVISION: &str = "staleRotationRevision";
const ERROR_MEMBER_MISSING: &str = "accountRotationMemberMissing";

impl SessionRuntimeEngine {
    pub(crate) async fn read_account_rotation(
        &self,
        params: ThreadAccountRotationReadParams,
    ) -> Result<ThreadAccountRotationReadResponse, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        self.ensure_account_rotation_thread_exists(thread_id)
            .await?;
        let policy = self
            .thread_store
            .thread_account_rotation_policy(thread_id)
            .await
            .map_err(|error| internal_error(format!("account rotation store failed: {error}")))?;
        Ok(ThreadAccountRotationReadResponse {
            rotation: api_snapshot(policy),
        })
    }

    pub(crate) async fn update_account_rotation(
        &self,
        params: ThreadAccountRotationUpdateParams,
    ) -> Result<ThreadAccountRotationUpdateResponse, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        self.ensure_account_rotation_thread_exists(thread_id)
            .await?;
        let slots = self.account_registry.rotation_slot_inventory().await?;
        let by_id = slots
            .iter()
            .map(|slot| (slot.account_slot_id.as_str(), slot.account_number))
            .collect::<HashMap<_, _>>();
        if params
            .fixed_account_slot_id
            .as_deref()
            .is_some_and(|slot_id| !by_id.contains_key(slot_id))
        {
            return Err(member_missing());
        }
        let mut seen = HashSet::with_capacity(params.automatic_account_slot_ids.len());
        if params
            .automatic_account_slot_ids
            .iter()
            .any(|slot_id| slot_id.is_empty() || !seen.insert(slot_id.as_str()))
        {
            return Err(invalid_params(
                "automatic account rotation slots must be non-empty and distinct",
            ));
        }
        if params
            .automatic_account_slot_ids
            .iter()
            .any(|slot_id| !by_id.contains_key(slot_id.as_str()))
        {
            return Err(member_missing());
        }
        if params.mode == ApiRotationMode::Fixed && params.fixed_account_slot_id.is_none() {
            return Err(invalid_params(
                "fixed account rotation requires a fixed account slot",
            ));
        }
        if params.mode != ApiRotationMode::Fixed && params.automatic_account_slot_ids.is_empty() {
            return Err(invalid_params(
                "automatic account rotation requires at least one account slot",
            ));
        }
        let mut automatic_account_slot_ids = params.automatic_account_slot_ids;
        automatic_account_slot_ids.sort_by_key(|slot_id| by_id[slot_id.as_str()]);
        let update = ThreadAccountRotationPolicyUpdate {
            mode: store_mode(params.mode),
            fixed_account_slot_id: params.fixed_account_slot_id,
            automatic_account_slot_ids,
        };
        let Some(policy) = self
            .thread_store
            .compare_and_swap_thread_account_rotation_policy(
                thread_id,
                params.expected_rotation_revision,
                update,
            )
            .await
            .map_err(|error| internal_error(format!("account rotation store failed: {error}")))?
        else {
            return Err(structured_invalid_request(
                ERROR_STALE_REVISION,
                "account rotation revision is stale",
            ));
        };
        self.publish_thread(thread_id).await;
        Ok(ThreadAccountRotationUpdateResponse {
            rotation: api_snapshot(policy),
        })
    }

    pub(super) async fn account_rotation_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> Option<ThreadAccountRotationSnapshot> {
        self.thread_store
            .thread_account_rotation_policy(thread_id)
            .await
            .ok()
            .map(api_snapshot)
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

fn parse_thread_id(thread_id: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(thread_id)
        .map_err(|_| invalid_params("thread account rotation threadId is invalid"))
}

fn member_missing() -> JSONRPCErrorError {
    structured_invalid_request(
        ERROR_MEMBER_MISSING,
        "account rotation member is unavailable",
    )
}

fn api_snapshot(policy: ThreadAccountRotationPolicy) -> ThreadAccountRotationSnapshot {
    ThreadAccountRotationSnapshot {
        mode: api_mode(policy.mode),
        fixed_account_slot_id: policy.fixed_account_slot_id,
        automatic_account_slot_ids: policy.automatic_account_slot_ids,
        revision: policy.revision,
        last_committed_account_slot_id: policy.last_committed_account_slot_id,
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
