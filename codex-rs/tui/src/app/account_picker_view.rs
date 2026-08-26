use super::account_picker::AccountControlIntent;
use super::account_picker::AccountLoginMethod;
use super::account_rotation::AccountRotationEdit;
use super::*;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotActionAvailability;
use codex_app_server_protocol::AccountSlotHealth;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SessionRuntimeAction;

pub(super) const ACCOUNT_PICKER_VIEW_ID: &str = "account-picker";
pub(super) const ACCOUNT_DETAIL_VIEW_ID: &str = "account-detail";

impl App {
    fn account_selection_view_params(&self, selected_slot_id: Option<&str>) -> SelectionViewParams {
        let current_slot_id = self.current_account_slot_id();
        let mut initial_selected_idx = None;
        let mut items = Vec::new();
        if let Some(summary) = self.account_rotation_summary() {
            items.push(SelectionItem {
                name: "Rotation settings".to_string(),
                description: Some(summary),
                actions: vec![Box::new(|tx: &AppEventSender| {
                    tx.send(AppEvent::OpenAccountRotation);
                })],
                ..Default::default()
            });
        }
        let account_row_offset = items.len();
        items.extend(self.account_slots.iter().enumerate().map(|(index, slot)| {
            let index = index + account_row_offset;
            let is_current = current_slot_id == Some(slot.account_slot_id.as_str());
            if selected_slot_id == Some(slot.account_slot_id.as_str())
                || selected_slot_id.is_none() && is_current
            {
                initial_selected_idx = Some(index);
            }
            let slot_id = slot.account_slot_id.clone();
            SelectionItem {
                name: account_slot_display_name(slot),
                description: Some(account_slot_status_label(slot)),
                is_current,
                is_default: slot.is_default,
                actions: vec![Box::new(move |tx: &AppEventSender| {
                    tx.send(AppEvent::OpenAccountDetail {
                        slot_id: slot_id.clone(),
                    });
                })],
                ..Default::default()
            }
        }));
        SelectionViewParams {
            view_id: Some(ACCOUNT_PICKER_VIEW_ID),
            title: Some("Accounts".to_string()),
            subtitle: Some(
                "Global accounts from TokenManager; selection policy and current execution are shown separately."
                    .to_string(),
            ),
            items,
            initial_selected_idx,
            ..Default::default()
        }
    }

    fn account_detail_view_params(&self, slot: &AccountSlotSnapshot) -> SelectionViewParams {
        let is_current = self.current_account_slot_id() == Some(slot.account_slot_id.as_str());
        let mut items = Vec::new();
        if slot.is_default {
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
                "Authenticate this local account",
                login_allowed,
                login_reason,
                move |tx| {
                    tx.send(AppEvent::OpenAccountLoginMethods {
                        slot_id: Some(slot_id.clone()),
                    });
                },
            );

            let retry = action_availability(slot, AccountSlotAction::RetryLogin);
            let retry_allowed = slot.status == AccountSlotStatus::Failed
                && retry.is_some_and(|action| action.allowed);
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
                "Retry the failed local sign-in",
                retry_allowed,
                retry_reason,
                move |tx| {
                    tx.send(AppEvent::OpenAccountLoginMethods {
                        slot_id: Some(slot_id.clone()),
                    });
                },
            );

            let reauthenticate_allowed = slot.status == AccountSlotStatus::Ready
                && retry.is_some_and(|action| action.allowed);
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
                "Replace this local account's credentials",
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
                "Stop the active local sign-in attempt",
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
        }

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

        if let Some(rotation) = self.account_rotation_snapshot() {
            let is_fixed_target =
                rotation.fixed_account_slot_id.as_deref() == Some(slot.account_slot_id.as_str());
            let slot_id = slot.account_slot_id.clone();
            push_account_action(
                &mut items,
                "Set as fixed account",
                "Use this account when rotation mode is fixed",
                !is_fixed_target,
                is_fixed_target.then(|| "Current fixed account".to_string()),
                move |tx| {
                    tx.send(AppEvent::EditAccountRotation {
                        edit: AccountRotationEdit::FixedSlot(slot_id.clone()),
                    });
                },
            );
        }

        if slot.is_default {
            let logout = action_availability(slot, AccountSlotAction::Logout);
            let logout_allowed = logout.is_some_and(|action| action.allowed);
            let logout_reason = logout
                .and_then(|action| action.deny_reason.clone())
                .or_else(|| (!logout_allowed).then(|| "Logout is unavailable".to_string()));
            let slot_id = slot.account_slot_id.clone();
            push_account_action(
                &mut items,
                "Log out",
                "Remove this local account's credentials",
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
        }

        SelectionViewParams {
            view_id: Some(ACCOUNT_DETAIL_VIEW_ID),
            title: Some(account_slot_display_name(slot)),
            subtitle: Some(format!(
                "{} · {}{}",
                account_slot_status_label(slot),
                if slot.is_default {
                    "Local account"
                } else {
                    "Global account"
                },
                if is_current { " · Current" } else { "" }
            )),
            items,
            ..Default::default()
        }
    }

    pub(super) fn current_account_slot_id(&self) -> Option<&str> {
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
        let account_row_offset = usize::from(self.account_rotation_snapshot().is_some());
        self.chat_widget
            .selected_index_for_present_view(ACCOUNT_PICKER_VIEW_ID)
            .and_then(|index| index.checked_sub(account_row_offset))
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
        let rotation_replaced = self.replace_account_rotation_view_if_present();
        if !list_replaced && !detail_replaced && !rotation_replaced {
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

pub(super) fn account_slot_display_name(slot: &AccountSlotSnapshot) -> String {
    format!("{}. {}", slot.account_number, slot.label)
}

pub(super) fn account_slot_status_label(slot: &AccountSlotSnapshot) -> String {
    let mut status = match slot.status {
        AccountSlotStatus::LoginRequired => "Login required",
        AccountSlotStatus::Ready => "Ready",
        AccountSlotStatus::Failed => "Login failed",
    }
    .to_string();
    if slot.active_login_operation_id.is_some() {
        status.push_str(" · Login in progress");
    }
    if let Some(error_code) = &slot.error_code {
        status.push_str(&format!(" · Error: {error_code}"));
    }
    status.push_str(match slot.health {
        AccountSlotHealth::Healthy => " · Projection healthy",
        AccountSlotHealth::Degraded => " · Projection stale",
        AccountSlotHealth::Unavailable => " · Projection unavailable",
    });
    if let Some(quota) = &slot.quota {
        for meter in &quota.meters {
            let label = meter.label.as_deref().unwrap_or(&meter.id);
            status.push_str(&format!(" · {label} {}% left", meter.remaining_percent));
            if let Some(resets_at) = meter.resets_at {
                status.push_str(&format!(", resets at {resets_at}"));
            }
        }
        status.push_str(&format!(" · quota fresh until {}", quota.stale_at));
    }
    status
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
