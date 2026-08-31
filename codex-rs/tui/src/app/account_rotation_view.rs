//! Selection-list presentation for per-thread account rotation.

use super::account_picker_view::account_slot_display_name;
use super::account_picker_view::account_slot_status_label;
use super::account_rotation::AccountRotationEdit;
use super::*;
use crate::bottom_pane::SelectionToggle;
use codex_app_server_protocol::AccountSlotHealth;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::ThreadAccountRotationMode;

pub(super) const ACCOUNT_ROTATION_VIEW_ID: &str = "account-rotation";

impl App {
    pub(super) fn account_rotation_summary(&self) -> Option<String> {
        let rotation = self.account_rotation_snapshot()?;
        Some(format!(
            "Desired: {} · Actual: {} · {} automatic account{}",
            account_rotation_mode_label(rotation.mode),
            self.current_account_slot_id().unwrap_or("unbound"),
            rotation.automatic_account_slot_ids.len(),
            if rotation.automatic_account_slot_ids.len() == 1 {
                ""
            } else {
                "s"
            }
        ))
    }

    pub(super) fn account_rotation_view_params(&self) -> Option<SelectionViewParams> {
        let rotation = self.account_rotation_snapshot()?;
        let current_slot_id = self.current_account_slot_id();
        let mut items = Vec::new();
        for mode in [
            ThreadAccountRotationMode::Fixed,
            ThreadAccountRotationMode::QuotaAware,
            ThreadAccountRotationMode::RoundRobin,
            ThreadAccountRotationMode::ExhaustThenNext,
        ] {
            let mode_allowed = match mode {
                ThreadAccountRotationMode::Fixed => rotation.fixed_account_slot_id.is_some(),
                ThreadAccountRotationMode::QuotaAware
                | ThreadAccountRotationMode::RoundRobin
                | ThreadAccountRotationMode::ExhaustThenNext => {
                    !rotation.automatic_account_slot_ids.is_empty()
                }
            };
            items.push(SelectionItem {
                name: format!("Mode: {}", account_rotation_mode_label(mode)),
                description: Some(if rotation.mode == mode {
                    "Selected rotation mode".to_string()
                } else if !mode_allowed {
                    match mode {
                        ThreadAccountRotationMode::Fixed => {
                            "Select a fixed account first".to_string()
                        }
                        ThreadAccountRotationMode::QuotaAware
                        | ThreadAccountRotationMode::RoundRobin
                        | ThreadAccountRotationMode::ExhaustThenNext => {
                            "Select at least one automatic account first".to_string()
                        }
                    }
                } else {
                    "Use this mode starting with the next user turn".to_string()
                }),
                is_current: rotation.mode == mode,
                is_disabled: !mode_allowed,
                disabled_reason: (!mode_allowed).then(|| match mode {
                    ThreadAccountRotationMode::Fixed => "Fixed account is not selected".to_string(),
                    ThreadAccountRotationMode::QuotaAware
                    | ThreadAccountRotationMode::RoundRobin
                    | ThreadAccountRotationMode::ExhaustThenNext => {
                        "Automatic membership is empty".to_string()
                    }
                }),
                actions: mode_allowed
                    .then(|| {
                        Box::new(move |tx: &AppEventSender| {
                            tx.send(AppEvent::EditAccountRotation {
                                edit: AccountRotationEdit::Mode(mode),
                            });
                        }) as crate::bottom_pane::SelectionAction
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            });
        }
        for slot in &self.account_slots {
            let slot_id = slot.account_slot_id.clone();
            let is_fixed = rotation.fixed_account_slot_id.as_deref() == Some(slot_id.as_str());
            items.push(SelectionItem {
                name: format!("Fixed: {}", account_slot_display_name(slot)),
                description: Some(format!(
                    "{}{}",
                    account_slot_status_label(slot),
                    if current_slot_id == Some(slot.account_slot_id.as_str()) {
                        " · Current runtime"
                    } else {
                        ""
                    }
                )),
                is_current: is_fixed,
                actions: vec![Box::new(move |tx: &AppEventSender| {
                    tx.send(AppEvent::EditAccountRotation {
                        edit: AccountRotationEdit::FixedSlot(slot_id.clone()),
                    });
                })],
                ..Default::default()
            });
        }
        for slot in &self.account_slots {
            let slot_id = slot.account_slot_id.clone();
            let is_member = rotation
                .automatic_account_slot_ids
                .contains(&slot.account_slot_id);
            let automatic_deny_reason = automatic_account_deny_reason(slot);
            items.push(SelectionItem {
                name: format!("Automatic: {}", account_slot_display_name(slot)),
                description: Some(account_slot_status_label(slot)),
                is_disabled: automatic_deny_reason.is_some() && !is_member,
                disabled_reason: if is_member {
                    None
                } else {
                    automatic_deny_reason
                },
                toggle: Some(SelectionToggle {
                    is_on: is_member,
                    action: Box::new(move |enabled, tx: &AppEventSender| {
                        tx.send(AppEvent::EditAccountRotation {
                            edit: AccountRotationEdit::AutomaticMembership {
                                slot_id: slot_id.clone(),
                                enabled,
                            },
                        });
                    }),
                }),
                ..Default::default()
            });
        }
        Some(SelectionViewParams {
            view_id: Some(ACCOUNT_ROTATION_VIEW_ID),
            title: Some("Account rotation".to_string()),
            subtitle: Some(format!(
                "Desired revision {} · actual execution changes only when a turn starts",
                rotation.revision,
            )),
            items,
            ..Default::default()
        })
    }

    pub(super) fn replace_account_rotation_view_if_present(&mut self) -> bool {
        let params = self
            .account_rotation_view_params()
            .unwrap_or_else(account_rotation_unavailable_view_params);
        self.chat_widget
            .replace_selection_view_if_present(ACCOUNT_ROTATION_VIEW_ID, params)
    }
}

fn automatic_account_deny_reason(slot: &AccountSlotSnapshot) -> Option<String> {
    if slot.status != AccountSlotStatus::Ready {
        return Some("Automatic selection requires a logged-in account".to_string());
    }
    match slot.health {
        AccountSlotHealth::Healthy => None,
        AccountSlotHealth::Degraded => {
            Some("Automatic selection is paused while this projection is stale".to_string())
        }
        AccountSlotHealth::Unavailable => {
            Some("Automatic selection is unavailable for this account".to_string())
        }
    }
}

pub(super) fn account_rotation_loading_view_params() -> SelectionViewParams {
    SelectionViewParams {
        view_id: Some(ACCOUNT_ROTATION_VIEW_ID),
        title: Some("Account rotation".to_string()),
        subtitle: Some("Loading authoritative rotation settings...".to_string()),
        items: vec![SelectionItem {
            name: "Loading...".to_string(),
            is_disabled: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn account_rotation_unavailable_view_params() -> SelectionViewParams {
    SelectionViewParams {
        view_id: Some(ACCOUNT_ROTATION_VIEW_ID),
        title: Some("Account rotation".to_string()),
        subtitle: Some("Rotation is unavailable for this app-server.".to_string()),
        items: vec![SelectionItem {
            name: "Rotation unavailable".to_string(),
            is_disabled: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

pub(super) fn account_rotation_mode_label(mode: ThreadAccountRotationMode) -> &'static str {
    match mode {
        ThreadAccountRotationMode::Fixed => "Fixed",
        ThreadAccountRotationMode::QuotaAware => "Quota aware",
        ThreadAccountRotationMode::RoundRobin => "Round robin",
        ThreadAccountRotationMode::ExhaustThenNext => "Exhaust then next",
    }
}

#[cfg(test)]
#[path = "account_rotation_view_tests.rs"]
mod tests;
