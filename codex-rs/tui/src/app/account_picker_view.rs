use super::account_picker::AccountControlIntent;
use super::account_picker::AccountLoginMethod;
use super::*;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotActionAvailability;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SessionRuntimeAction;

pub(super) const ACCOUNT_PICKER_VIEW_ID: &str = "account-picker";
pub(super) const ACCOUNT_DETAIL_VIEW_ID: &str = "account-detail";

impl App {
    fn account_selection_view_params(&self, selected_slot_id: Option<&str>) -> SelectionViewParams {
        let current_slot_id = self.current_account_slot_id();
        let mut initial_selected_idx = None;
        let mut items = self
            .account_slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let is_current = current_slot_id == Some(slot.account_slot_id.as_str());
                if selected_slot_id == Some(slot.account_slot_id.as_str())
                    || selected_slot_id.is_none() && is_current
                {
                    initial_selected_idx = Some(index);
                }
                let slot_id = slot.account_slot_id.clone();
                SelectionItem {
                    name: slot.label.clone(),
                    description: Some(account_slot_status_label(slot).to_string()),
                    is_current,
                    is_default: slot.is_default,
                    actions: vec![Box::new(move |tx: &AppEventSender| {
                        tx.send(AppEvent::OpenAccountDetail {
                            slot_id: slot_id.clone(),
                        });
                    })],
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>();
        let add_allowed = self
            .account_slot_capability
            .as_ref()
            .is_some_and(|capability| capability.available);
        items.push(SelectionItem {
            name: "Add account".to_string(),
            description: Some("Sign in with a browser or device code".to_string()),
            is_disabled: !add_allowed,
            disabled_reason: self
                .account_slot_capability
                .as_ref()
                .and_then(|capability| capability.deny_reason.clone()),
            actions: add_allowed
                .then(|| {
                    Box::new(|tx: &AppEventSender| {
                        tx.send(AppEvent::OpenAccountLoginMethods { slot_id: None });
                    }) as crate::bottom_pane::SelectionAction
                })
                .into_iter()
                .collect(),
            dismiss_on_select: true,
            ..Default::default()
        });
        SelectionViewParams {
            view_id: Some(ACCOUNT_PICKER_VIEW_ID),
            title: Some("Accounts".to_string()),
            subtitle: Some("Select an account to view its available actions.".to_string()),
            items,
            initial_selected_idx,
            ..Default::default()
        }
    }

    fn account_detail_view_params(&self, slot: &AccountSlotSnapshot) -> SelectionViewParams {
        let is_current = self.current_account_slot_id() == Some(slot.account_slot_id.as_str());
        let mut items = Vec::new();
        let login = action_availability(slot, AccountSlotAction::Login);
        let login_allowed = slot.status == AccountSlotStatus::LoginRequired
            && login.is_some_and(|action| action.allowed);
        let login_reason = status_action_deny_reason(
            slot,
            AccountSlotStatus::LoginRequired,
            login,
            "Account does not require login",
        );
        let slot_id = slot.account_slot_id.clone();
        push_account_action(
            &mut items,
            "Log in",
            "Authenticate this account",
            login_allowed,
            login_reason,
            move |tx| {
                tx.send(AppEvent::OpenAccountLoginMethods {
                    slot_id: Some(slot_id.clone()),
                });
            },
        );

        let retry = action_availability(slot, AccountSlotAction::RetryLogin);
        let retry_allowed =
            slot.status == AccountSlotStatus::Failed && retry.is_some_and(|action| action.allowed);
        let retry_reason = status_action_deny_reason(
            slot,
            AccountSlotStatus::Failed,
            retry,
            "No failed login to retry",
        );
        let slot_id = slot.account_slot_id.clone();
        push_account_action(
            &mut items,
            "Retry login",
            "Retry the failed sign-in",
            retry_allowed,
            retry_reason,
            move |tx| {
                tx.send(AppEvent::OpenAccountLoginMethods {
                    slot_id: Some(slot_id.clone()),
                });
            },
        );

        let reauthenticate_allowed =
            slot.status == AccountSlotStatus::Ready && retry.is_some_and(|action| action.allowed);
        let reauthenticate_reason = status_action_deny_reason(
            slot,
            AccountSlotStatus::Ready,
            retry,
            "Account is not ready",
        );
        let slot_id = slot.account_slot_id.clone();
        push_account_action(
            &mut items,
            "Sign in again",
            "Replace this account's credentials",
            reauthenticate_allowed,
            reauthenticate_reason,
            move |tx| {
                tx.send(AppEvent::OpenAccountLoginMethods {
                    slot_id: Some(slot_id.clone()),
                });
            },
        );

        let active_login = slot.active_login_operation_id.clone();
        let cancel_allowed = active_login.is_some();
        let slot_id = slot.account_slot_id.clone();
        push_account_action(
            &mut items,
            "Cancel login",
            "Stop the active sign-in attempt",
            cancel_allowed,
            (!cancel_allowed).then(|| "No login is in progress".to_string()),
            move |tx| {
                if let Some(login_id) = &active_login {
                    tx.send(AppEvent::CancelAccountLogin {
                        slot_id: slot_id.clone(),
                        login_id: login_id.clone(),
                    });
                }
            },
        );

        let slot_switch = action_availability(slot, AccountSlotAction::SwitchTo);
        let runtime_switch = self.account_runtime.as_ref().and_then(|(_, runtime)| {
            runtime
                .actions
                .iter()
                .find(|action| action.action == SessionRuntimeAction::SwitchAccount)
        });
        let switch_allowed = !is_current
            && slot_switch.is_some_and(|action| action.allowed)
            && runtime_switch.is_some_and(|action| action.allowed);
        let switch_reason = is_current
            .then(|| "Current account".to_string())
            .or_else(|| slot_switch.and_then(|action| action.deny_reason.clone()))
            .or_else(|| runtime_switch.and_then(|action| action.deny_reason.clone()))
            .or_else(|| (!switch_allowed).then(|| "Account switching is unavailable".to_string()));
        let slot_id = slot.account_slot_id.clone();
        push_account_action(
            &mut items,
            "Use for this session",
            "Switch the next turn to this account",
            switch_allowed,
            switch_reason,
            move |tx| {
                tx.send(AppEvent::PrepareAccountControl {
                    intent: AccountControlIntent::Switch {
                        slot_id: slot_id.clone(),
                    },
                });
            },
        );

        let logout = action_availability(slot, AccountSlotAction::Logout);
        let logout_allowed = logout.is_some_and(|action| action.allowed);
        let logout_reason = logout
            .and_then(|action| action.deny_reason.clone())
            .or_else(|| (!logout_allowed).then(|| "Logout is unavailable".to_string()));
        let slot_id = slot.account_slot_id.clone();
        push_account_action(
            &mut items,
            "Log out",
            "Remove this account's credentials",
            logout_allowed,
            logout_reason,
            move |tx| {
                tx.send(AppEvent::PrepareAccountControl {
                    intent: AccountControlIntent::Logout {
                        slot_id: slot_id.clone(),
                    },
                });
            },
        );

        SelectionViewParams {
            view_id: Some(ACCOUNT_DETAIL_VIEW_ID),
            title: Some(slot.label.clone()),
            subtitle: Some(format!(
                "{} · {}{}{}",
                account_slot_status_label(slot),
                if slot.is_default {
                    "Default account"
                } else {
                    "Secondary account"
                },
                if is_current { " · Current" } else { "" },
                slot.error_code
                    .as_ref()
                    .map(|code| format!(" · Error: {code}"))
                    .unwrap_or_default()
            )),
            items,
            ..Default::default()
        }
    }

    fn current_account_slot_id(&self) -> Option<&str> {
        self.account_runtime
            .as_ref()
            .and_then(|(_, runtime)| runtime.account.current.as_ref())
            .map(|account| account.account_slot_id.as_str())
    }

    pub(super) fn show_account_detail(&mut self, slot_id: String) {
        let Some(slot) = self
            .account_slots
            .iter()
            .find(|slot| slot.account_slot_id == slot_id)
        else {
            self.chat_widget
                .add_error_message("The selected account no longer exists.".to_string());
            return;
        };
        let params = self.account_detail_view_params(slot);
        self.account_detail_slot_id = Some(slot_id);
        self.chat_widget.show_selection_view(params);
    }

    pub(super) fn selected_account_slot_id(&self) -> Option<String> {
        self.chat_widget
            .selected_index_for_present_view(ACCOUNT_PICKER_VIEW_ID)
            .and_then(|index| self.account_slots.get(index))
            .map(|slot| slot.account_slot_id.clone())
    }

    pub(super) fn replace_account_picker_if_present(
        &mut self,
        selected_slot_id: Option<&str>,
    ) -> bool {
        self.chat_widget.replace_selection_view_if_present(
            ACCOUNT_PICKER_VIEW_ID,
            self.account_selection_view_params(selected_slot_id),
        )
    }

    pub(super) fn replace_open_account_views(&mut self, selected_slot_id: Option<&str>) {
        let list_replaced = self.chat_widget.replace_selection_view_if_present(
            ACCOUNT_PICKER_VIEW_ID,
            self.account_selection_view_params(selected_slot_id),
        );
        let detail_params = self.account_detail_slot_id.as_ref().and_then(|slot_id| {
            self.account_slots
                .iter()
                .find(|slot| slot.account_slot_id == *slot_id)
                .map(|slot| self.account_detail_view_params(slot))
        });
        let detail_replaced = detail_params.is_some_and(|params| {
            self.chat_widget
                .replace_selection_view_if_present(ACCOUNT_DETAIL_VIEW_ID, params)
        });
        if !list_replaced && !detail_replaced {
            self.account_detail_slot_id = None;
        }
    }

    pub(super) fn show_account_login_methods(&mut self, slot_id: Option<String>) {
        let browser_slot = slot_id.clone();
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Account login".to_string()),
            subtitle: Some("Choose a sign-in method.".to_string()),
            items: vec![
                SelectionItem {
                    name: "Continue in browser".to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::PrepareAccountControl {
                            intent: AccountControlIntent::Login {
                                slot_id: browser_slot.clone(),
                                method: AccountLoginMethod::Browser,
                            },
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Use device code".to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::PrepareAccountControl {
                            intent: AccountControlIntent::Login {
                                slot_id: slot_id.clone(),
                                method: AccountLoginMethod::DeviceCode,
                            },
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
    }
}

fn account_slot_status_label(slot: &AccountSlotSnapshot) -> &'static str {
    if slot.active_login_operation_id.is_some() {
        return "Login in progress";
    }
    if slot
        .error_code
        .as_deref()
        .is_some_and(|code| matches!(code, "authUnavailable" | "refreshUnavailable"))
    {
        return "Unavailable";
    }
    match slot.status {
        AccountSlotStatus::LoginRequired => "Login required",
        AccountSlotStatus::Ready => "Ready",
        AccountSlotStatus::Failed => "Login failed",
    }
}

fn action_availability(
    slot: &AccountSlotSnapshot,
    action: AccountSlotAction,
) -> Option<&AccountSlotActionAvailability> {
    slot.actions
        .iter()
        .find(|availability| availability.action == action)
}

fn status_action_deny_reason(
    slot: &AccountSlotSnapshot,
    expected_status: AccountSlotStatus,
    availability: Option<&AccountSlotActionAvailability>,
    status_reason: &str,
) -> Option<String> {
    if slot.status != expected_status {
        return Some(status_reason.to_string());
    }
    availability
        .and_then(|availability| availability.deny_reason.clone())
        .or_else(|| {
            availability
                .is_none()
                .then(|| "Action is unavailable".to_string())
        })
}

fn push_account_action(
    items: &mut Vec<SelectionItem>,
    name: &str,
    description: &str,
    allowed: bool,
    disabled_reason: Option<String>,
    action: impl Fn(&AppEventSender) + Send + Sync + 'static,
) {
    items.push(SelectionItem {
        name: name.to_string(),
        description: Some(description.to_string()),
        is_disabled: !allowed,
        disabled_reason,
        actions: allowed
            .then(|| Box::new(action) as crate::bottom_pane::SelectionAction)
            .into_iter()
            .collect(),
        dismiss_on_select: true,
        ..Default::default()
    });
}

#[cfg(test)]
#[path = "account_picker_view_tests.rs"]
mod tests;
