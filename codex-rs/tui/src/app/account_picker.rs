//! Account-slot picker and exact-thread account controls.

use super::*;
use crate::app_server_session::AccountSlotsSnapshot;
use crate::app_server_session::ThreadRuntimeSnapshot;
use crate::app_server_session::list_account_slots;
use crate::app_server_session::session_runtime_for_thread;

const ACCOUNT_PICKER_VIEW_ID: &str = "account-picker";

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
        prior_registry_revision: u64,
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
        self.account_registry_revision = snapshot.slots.registry_revision;
        self.account_slots = snapshot.slots.data;
        self.account_slot_capability = Some(snapshot.slots.multi_account);
        self.account_runtime = Some((snapshot.runtime.instance_epoch, snapshot.runtime.snapshot));
        if self
            .pending_account_control
            .as_ref()
            .is_some_and(PendingAccountControl::validation_in_flight)
        {
            self.finish_account_control_validation();
        }
        self.show_account_picker();
    }

    pub(super) fn next_account_request_generation(&mut self) -> u64 {
        self.account_request_generation = self.account_request_generation.saturating_add(1);
        self.account_request_generation
    }
}
