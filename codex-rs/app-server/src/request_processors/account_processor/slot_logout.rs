use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;

use super::AccountRequestProcessor;
use crate::account_registry::live_registration::structured_invalid_request;

const ERROR_SLOT_BOUND: &str = "accountSlotBound";

impl AccountRequestProcessor {
    pub(crate) async fn logout_account_slot(
        &self,
        params: AccountSlotLogoutParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if params.account_slot_id == "default" {
            return Err(crate::error_code::invalid_request(
                "default account logout must use account/logout",
            ));
        }
        if self
            .session_runtime
            .account_slot_in_use(&params.account_slot_id)
            .await?
        {
            return Err(structured_invalid_request(
                ERROR_SLOT_BOUND,
                "account slot is bound to a thread",
            ));
        }

        let logged_out = self.account_registry.logout_secondary(params).await?;
        self.thread_manager.clear_account_plugin_cache(
            &logged_out.response.slot.account_slot_id,
            &logged_out.runtime.auth_manager,
        );
        self.forget_external_slot_owner(&logged_out.response.slot.account_slot_id)
            .await;
        self.outgoing
            .send_server_notification(ServerNotification::AccountSlotChanged(
                logged_out.notification,
            ))
            .await;
        Ok(Some(logged_out.response.into()))
    }
}
