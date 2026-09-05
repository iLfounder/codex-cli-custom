use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_app_server_protocol::AccountSlotLogoutResponse;
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
        if self.account_registry.global_managed_mode()? {
            let account_slot_id = params.account_slot_id.clone();
            self.account_registry.logout_global_account(&params).await?;
            self.account_registry
                .notify_global_inventory_if_changed(&self.outgoing)
                .await;
            let slot = self
                .account_registry
                .global_slot_snapshot(&account_slot_id)
                .await?;
            return Ok(Some(AccountSlotLogoutResponse { slot }.into()));
        }
        if params.account_slot_id == "default" {
            return Err(crate::error_code::invalid_request(
                "default account logout must use account/logout",
            ));
        }
        let account_slot_id = params.account_slot_id.clone();
        let reservation = self
            .account_registry
            .reserve_secondary_logout(params)
            .await?;
        let slot_in_use = match self
            .session_runtime
            .account_slot_in_use(&account_slot_id)
            .await
        {
            Ok(slot_in_use) => slot_in_use,
            Err(error) => {
                let _ = self
                    .account_registry
                    .clear_logout_reservation(&reservation)
                    .await;
                return Err(error);
            }
        };
        if slot_in_use {
            self.account_registry
                .clear_logout_reservation(&reservation)
                .await?;
            return Err(structured_invalid_request(
                ERROR_SLOT_BOUND,
                "account slot is bound to a thread",
            ));
        }

        let logged_out = self
            .account_registry
            .logout_reserved_secondary(reservation)
            .await?;
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
