//! Per-thread account rotation reads, revisioned updates, and event integration.

use super::account_rotation_view::account_rotation_loading_view_params;
use super::*;
use crate::app_server_session::read_thread_account_rotation;
use crate::app_server_session::update_thread_account_rotation;
use codex_app_server_protocol::ThreadAccountRotationMode;
use codex_app_server_protocol::ThreadAccountRotationSnapshot;
use codex_app_server_protocol::ThreadAccountRotationUpdateParams;
use codex_app_server_protocol::ThreadAccountRotationUpdateResponse;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccountRotationEdit {
    Mode(ThreadAccountRotationMode),
    FixedSlot(String),
    AutomaticMembership { slot_id: String, enabled: bool },
}

impl App {
    pub(super) fn account_rotation_snapshot(&self) -> Option<&ThreadAccountRotationSnapshot> {
        if !self.account_rotation_available {
            return None;
        }
        self.account_runtime
            .as_ref()
            .and_then(|(_, runtime)| runtime.account.rotation.as_ref())
    }

    pub(super) fn open_account_rotation_editor(&mut self, app_server: &AppServerSession) {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            return;
        };
        if self.account_rotation_snapshot().is_none() {
            self.chat_widget.add_error_message(
                "Account rotation is unavailable for this app-server.".to_string(),
            );
            return;
        }
        self.account_rotation_request_generation =
            self.account_rotation_request_generation.saturating_add(1);
        let request_generation = self.account_rotation_request_generation;
        self.chat_widget
            .show_selection_view(account_rotation_loading_view_params());
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = read_thread_account_rotation(request_handle, thread_id)
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::AccountRotationLoaded {
                thread_id,
                request_generation,
                result,
            });
        });
    }

    pub(super) fn handle_account_rotation_loaded(
        &mut self,
        thread_id: ThreadId,
        request_generation: u64,
        result: Result<codex_app_server_protocol::ThreadAccountRotationReadResponse, String>,
    ) {
        if request_generation != self.account_rotation_request_generation
            || self.current_displayed_thread_id() != Some(thread_id)
        {
            return;
        }
        match result {
            Ok(response) => {
                self.apply_account_rotation(thread_id, response.rotation);
                self.replace_account_rotation_view_if_present();
            }
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Could not read account rotation: {error}"));
                self.replace_account_rotation_view_if_present();
            }
        }
    }

    pub(super) fn edit_account_rotation(
        &mut self,
        app_server: &AppServerSession,
        edit: AccountRotationEdit,
    ) {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            return;
        };
        let Some(rotation) = self.account_rotation_snapshot().cloned() else {
            self.chat_widget.add_error_message(
                "Account rotation is unavailable for this app-server.".to_string(),
            );
            return;
        };
        let expected_rotation_revision = rotation.revision;
        let mut mode = rotation.mode;
        let mut fixed_account_slot_id = rotation.fixed_account_slot_id;
        let mut automatic_account_slot_ids = rotation.automatic_account_slot_ids;
        match edit {
            AccountRotationEdit::Mode(next_mode) => mode = next_mode,
            AccountRotationEdit::FixedSlot(slot_id) => {
                fixed_account_slot_id = Some(slot_id);
            }
            AccountRotationEdit::AutomaticMembership { slot_id, enabled } => {
                let mut selected = automatic_account_slot_ids
                    .into_iter()
                    .collect::<HashSet<_>>();
                if enabled {
                    selected.insert(slot_id);
                } else {
                    selected.remove(&slot_id);
                }
                automatic_account_slot_ids = self
                    .account_slots
                    .iter()
                    .filter(|slot| selected.contains(&slot.account_slot_id))
                    .map(|slot| slot.account_slot_id.clone())
                    .collect();
            }
        }
        let params = ThreadAccountRotationUpdateParams {
            thread_id: thread_id.to_string(),
            expected_rotation_revision,
            mode,
            fixed_account_slot_id,
            automatic_account_slot_ids,
        };
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = update_thread_account_rotation(request_handle, params)
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::AccountRotationUpdated {
                thread_id,
                expected_rotation_revision,
                result,
            });
        });
    }

    pub(super) fn handle_account_rotation_updated(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        expected_rotation_revision: u64,
        result: Result<ThreadAccountRotationUpdateResponse, String>,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        match result {
            Ok(response) => {
                self.apply_account_rotation(thread_id, response.rotation);
                let selected_slot_id = self.selected_account_slot_id();
                self.replace_open_account_views(selected_slot_id.as_deref());
                self.chat_widget
                    .add_info_message("Account rotation updated.".to_string(), /*hint*/ None);
            }
            Err(error) => {
                self.chat_widget.add_error_message(format!(
                    "Could not update account rotation revision {expected_rotation_revision}: {error}"
                ));
                self.open_account_rotation_editor(app_server);
            }
        }
    }

    fn apply_account_rotation(
        &mut self,
        thread_id: ThreadId,
        rotation: ThreadAccountRotationSnapshot,
    ) {
        if !self.account_rotation_available {
            return;
        }
        let Some((_, runtime)) = self.account_runtime.as_mut() else {
            return;
        };
        if runtime.thread_id != thread_id.to_string()
            || runtime
                .account
                .rotation
                .as_ref()
                .is_some_and(|current| current.revision > rotation.revision)
        {
            return;
        }
        runtime.account.rotation = Some(rotation);
    }
}
