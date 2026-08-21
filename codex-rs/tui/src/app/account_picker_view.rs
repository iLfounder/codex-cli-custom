//! Account picker presentation and selection actions.

use super::account_picker::AccountControlIntent;
use super::account_picker::AccountLoginMethod;
use super::*;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SessionRuntimeAction;

const ACCOUNT_PICKER_VIEW_ID: &str = "account-picker";

impl App {
    fn account_selection_view_params(&self) -> SelectionViewParams {
        let current_slot_id = self
            .account_runtime
            .as_ref()
            .and_then(|(_, runtime)| runtime.account.current.as_ref())
            .map(|account| account.account_slot_id.as_str());
        let runtime_switch = self.account_runtime.as_ref().and_then(|(_, runtime)| {
            runtime
                .actions
                .iter()
                .find(|action| action.action == SessionRuntimeAction::SwitchAccount)
        });
        let mut items = Vec::new();
        let mut initial_selected_idx = None;
        for slot in &self.account_slots {
            let is_current = current_slot_id == Some(slot.account_slot_id.as_str());
            if is_current {
                initial_selected_idx = Some(items.len());
            }
            let primary_action = match slot.status {
                AccountSlotStatus::LoginRequired => AccountSlotAction::Login,
                AccountSlotStatus::Failed => AccountSlotAction::RetryLogin,
                AccountSlotStatus::Ready => AccountSlotAction::SwitchTo,
            };
            let availability = slot
                .actions
                .iter()
                .find(|action| action.action == primary_action);
            let allowed = if slot.status == AccountSlotStatus::Ready {
                !is_current
                    && availability.is_some_and(|action| action.allowed)
                    && runtime_switch.is_some_and(|action| action.allowed)
            } else {
                availability.is_some_and(|action| action.allowed)
                    && slot.active_login_operation_id.is_none()
            };
            let slot_id = slot.account_slot_id.clone();
            let action = allowed.then(|| {
                if slot.status == AccountSlotStatus::Ready {
                    Box::new(move |tx: &AppEventSender| {
                        tx.send(AppEvent::PrepareAccountControl {
                            intent: AccountControlIntent::Switch {
                                slot_id: slot_id.clone(),
                            },
                        });
                    }) as crate::bottom_pane::SelectionAction
                } else {
                    Box::new(move |tx: &AppEventSender| {
                        tx.send(AppEvent::OpenAccountLoginMethods {
                            slot_id: Some(slot_id.clone()),
                        });
                    }) as crate::bottom_pane::SelectionAction
                }
            });
            items.push(SelectionItem {
                name: slot.label.clone(),
                description: Some(
                    match slot.status {
                        AccountSlotStatus::LoginRequired => "Login required",
                        AccountSlotStatus::Ready => "Ready",
                        AccountSlotStatus::Failed => "Login failed",
                    }
                    .to_string(),
                ),
                is_current,
                is_default: slot.is_default,
                is_disabled: !allowed,
                disabled_reason: is_current
                    .then(|| "Current account".to_string())
                    .or_else(|| availability.and_then(|action| action.deny_reason.clone()))
                    .or_else(|| runtime_switch.and_then(|action| action.deny_reason.clone())),
                actions: action.into_iter().collect(),
                dismiss_on_select: true,
                ..Default::default()
            });

            let logout = slot
                .actions
                .iter()
                .find(|action| action.action == AccountSlotAction::Logout);
            if !slot.is_default && slot.status == AccountSlotStatus::Ready {
                let logout_allowed = logout.is_some_and(|action| action.allowed) && !is_current;
                let logout_slot_id = slot.account_slot_id.clone();
                items.push(SelectionItem {
                    name: format!("Log out {}", slot.label),
                    description: Some("Remove this secondary account".to_string()),
                    is_disabled: !logout_allowed,
                    disabled_reason: is_current
                        .then(|| "This session is using the account".to_string())
                        .or_else(|| logout.and_then(|action| action.deny_reason.clone())),
                    actions: logout_allowed
                        .then(|| {
                            Box::new(move |tx: &AppEventSender| {
                                tx.send(AppEvent::PrepareAccountControl {
                                    intent: AccountControlIntent::Logout {
                                        slot_id: logout_slot_id.clone(),
                                    },
                                });
                            }) as crate::bottom_pane::SelectionAction
                        })
                        .into_iter()
                        .collect(),
                    dismiss_on_select: true,
                    ..Default::default()
                });
            }
        }
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
            subtitle: Some("Choose the account used by the next turn.".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx,
            ..Default::default()
        }
    }

    pub(super) fn show_account_picker(&mut self) {
        let params = self.account_selection_view_params();
        if !self
            .chat_widget
            .replace_selection_view_if_present(ACCOUNT_PICKER_VIEW_ID, params)
        {
            self.chat_widget
                .show_selection_view(self.account_selection_view_params());
        }
    }

    pub(super) fn show_account_login_methods(&mut self, slot_id: Option<String>) {
        let browser_slot = slot_id.clone();
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Account login".to_string()),
            subtitle: Some("Choose a sign-in method.".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
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

#[cfg(test)]
#[path = "account_picker_view_tests.rs"]
mod tests;
