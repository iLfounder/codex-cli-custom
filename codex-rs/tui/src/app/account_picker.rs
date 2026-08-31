//! Account-slot picker and exact-thread account controls.

use super::account_picker_view::ACCOUNT_PICKER_VIEW_ID;
use super::*;
use crate::app_server_session::AccountSlotsSnapshot;
use crate::app_server_session::ThreadRuntimeSnapshot;
use crate::app_server_session::list_account_slots;
use crate::app_server_session::session_runtime_for_thread;
use codex_app_server_protocol::SESSION_RUNTIME_ACCOUNT_ROTATION_CAPABILITY;

#[derive(Debug)]
pub(crate) struct AccountPickerSnapshot {
    pub(crate) slots: AccountSlotsSnapshot,
    pub(crate) runtime: ThreadRuntimeSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountLoginMethod {
    Browser,
    DeviceCode,
}

#[derive(Debug)]
pub(crate) enum AccountControlIntent {
    Login {
        slot_id: Option<String>,
        method: AccountLoginMethod,
    },
    Switch {
        slot_id: String,
    },
    Logout {
        slot_id: String,
    },
}

#[derive(Debug)]
pub(super) enum PendingAccountControl {
    Login {
        operation_id: String,
        thread_id: ThreadId,
        target_slot_id: String,
        instance_epoch: String,
        attempt_generation: u64,
        minimum_registry_revision: u64,
        validation_in_flight: bool,
    },
    Switch {
        operation_id: String,
        thread_id: ThreadId,
        target_slot_id: String,
        instance_epoch: String,
        ready_state_revision: Option<u64>,
        ready_generation: Option<u64>,
        validation_in_flight: bool,
    },
    Logout {
        thread_id: ThreadId,
        target_slot_id: String,
        instance_epoch: String,
        minimum_registry_revision: u64,
        prior_generation: u64,
        validation_in_flight: bool,
    },
}

impl PendingAccountControl {
    pub(super) fn validation_in_flight(&self) -> bool {
        match self {
            Self::Login {
                validation_in_flight,
                ..
            }
            | Self::Switch {
                validation_in_flight,
                ..
            }
            | Self::Logout {
                validation_in_flight,
                ..
            } => *validation_in_flight,
        }
    }
}

impl App {
    pub(super) fn open_account_picker(&mut self, app_server: &AppServerSession) {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            self.chat_widget
                .add_error_message("A session must start before choosing an account.".to_string());
            return;
        };
        let request_generation = self.next_account_request_generation();
        self.chat_widget.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_PICKER_VIEW_ID),
            title: Some("Accounts".to_string()),
            subtitle: Some("Loading exact account and session state...".to_string()),
            items: vec![SelectionItem {
                name: "Loading...".to_string(),
                is_disabled: true,
                ..Default::default()
            }],
            ..Default::default()
        });
        let request_handle = app_server.request_handle();
        let runtime_handle = request_handle.clone();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let (slots, runtime) = tokio::join!(
                list_account_slots(request_handle),
                session_runtime_for_thread(runtime_handle, thread_id)
            );
            let result = match (slots, runtime) {
                (Ok(slots), Ok(runtime)) => Ok(AccountPickerSnapshot { slots, runtime }),
                (Err(error), _) | (_, Err(error)) => Err(error.to_string()),
            };
            app_event_tx.send(AppEvent::AccountPickerLoaded {
                thread_id,
                request_generation,
                result,
            });
        });
    }

    pub(super) fn refresh_account_state(&mut self, app_server: &AppServerSession) {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            return;
        };
        let request_generation = self.next_account_request_generation();
        let request_handle = app_server.request_handle();
        let runtime_handle = request_handle.clone();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let (slots, runtime) = tokio::join!(
                list_account_slots(request_handle),
                session_runtime_for_thread(runtime_handle, thread_id)
            );
            let result = match (slots, runtime) {
                (Ok(slots), Ok(runtime)) => Ok(AccountPickerSnapshot { slots, runtime }),
                (Err(error), _) | (_, Err(error)) => Err(error.to_string()),
            };
            app_event_tx.send(AppEvent::AccountStateRefreshed {
                thread_id,
                request_generation,
                result,
            });
        });
    }

    pub(super) fn handle_account_picker_loaded(
        &mut self,
        thread_id: ThreadId,
        request_generation: u64,
        result: Result<AccountPickerSnapshot, String>,
    ) {
        if request_generation != self.account_request_generation
            || self.current_displayed_thread_id() != Some(thread_id)
        {
            return;
        }
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.pending_account_control = None;
                self.chat_widget
                    .add_error_message(format!("Could not refresh account state: {error}"));
                return;
            }
        };
        self.apply_account_snapshot(snapshot);
        self.replace_account_picker_if_present(None);
    }

    pub(super) fn handle_account_state_refreshed(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        request_generation: u64,
        result: Result<AccountPickerSnapshot, String>,
    ) {
        if request_generation != self.account_request_generation
            || self.current_displayed_thread_id() != Some(thread_id)
        {
            return;
        }
        match result {
            Ok(snapshot) => {
                let selected_slot_id = self.selected_account_slot_id();
                let snapshot_is_fresh = self.apply_account_snapshot(snapshot);
                if !snapshot_is_fresh
                    && self
                        .pending_account_control
                        .as_ref()
                        .is_some_and(PendingAccountControl::validation_in_flight)
                {
                    self.refresh_account_state(app_server);
                    return;
                }
                self.replace_open_account_views(selected_slot_id.as_deref());
            }
            Err(error) => tracing::warn!("could not refresh account state: {error}"),
        }
    }

    fn apply_account_snapshot(&mut self, mut snapshot: AccountPickerSnapshot) -> bool {
        let rotation_available = snapshot.runtime.capabilities.iter().any(|capability| {
            capability.name == SESSION_RUNTIME_ACCOUNT_ROTATION_CAPABILITY && capability.available
        });
        self.account_rotation_available = rotation_available;
        if !rotation_available {
            snapshot.runtime.snapshot.account.rotation = None;
            if let Some((_, runtime)) = self.account_runtime.as_mut() {
                runtime.account.rotation = None;
            }
        }
        let same_runtime_epoch = self
            .account_runtime
            .as_ref()
            .is_some_and(|(epoch, _)| epoch == &snapshot.runtime.instance_epoch);
        let slots_are_fresh = !same_runtime_epoch
            || super::account_validation::revision_meets_lower_bound(
                snapshot.slots.registry_revision,
                self.account_registry_revision,
            );
        let runtime_is_fresh = !same_runtime_epoch
            || super::account_validation::runtime_revision_meets_lower_bound(
                self.account_runtime
                    .as_ref()
                    .map(|(epoch, runtime)| (epoch.as_str(), runtime.state_revision)),
                (
                    snapshot.runtime.instance_epoch.as_str(),
                    snapshot.runtime.snapshot.state_revision,
                ),
            );
        if slots_are_fresh {
            self.account_registry_revision = snapshot.slots.registry_revision;
            self.account_slots = snapshot.slots.data;
            self.account_slot_capability = Some(snapshot.slots.multi_account);
        }
        if runtime_is_fresh {
            self.account_runtime =
                Some((snapshot.runtime.instance_epoch, snapshot.runtime.snapshot));
        }
        let snapshot_is_fresh = slots_are_fresh && runtime_is_fresh;
        if snapshot_is_fresh
            && self
                .pending_account_control
                .as_ref()
                .is_some_and(PendingAccountControl::validation_in_flight)
        {
            self.finish_account_control_validation();
        }
        snapshot_is_fresh
    }

    pub(super) fn next_account_request_generation(&mut self) -> u64 {
        self.account_request_generation = self.account_request_generation.saturating_add(1);
        self.account_request_generation
    }
}

#[cfg(test)]
#[path = "account_picker_tests.rs"]
mod tests;
