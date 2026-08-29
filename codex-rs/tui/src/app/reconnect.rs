//! Canonical app-server reconnect and thread projection resynchronization.

use super::App;
use super::session_lifecycle::ThreadAttachPresentation;
use crate::AppServerTarget;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::ResumeModelSettings;
use crate::chatwidget::ThreadInputState;
use crate::chatwidget::ThreadInputStateRestoreMode;
use crate::tui;
use codex_app_server_client::AppServerInstanceIdentity;
use codex_protocol::ThreadId;

pub(super) struct PendingReconnect {
    thread_id: Option<ThreadId>,
    input_state: Option<ThreadInputState>,
}

pub(super) enum ConnectedDisposition {
    Baseline,
    Stale,
    Resync(Box<PendingReconnect>),
}

#[derive(Default)]
pub(super) struct ReconnectState {
    highest_identity: Option<AppServerInstanceIdentity>,
    pending: Option<PendingReconnect>,
}

impl ReconnectState {
    pub(super) fn is_disconnected(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn begin_disconnect(
        &mut self,
        thread_id: Option<ThreadId>,
        input_state: Option<ThreadInputState>,
    ) {
        if self.pending.is_none() {
            self.pending = Some(PendingReconnect {
                thread_id,
                input_state,
            });
        }
    }

    pub(super) fn observe_connected(
        &mut self,
        identity: AppServerInstanceIdentity,
    ) -> ConnectedDisposition {
        if let Some(highest_identity) = self.highest_identity {
            if identity == highest_identity {
                return self
                    .pending
                    .take()
                    .map_or(ConnectedDisposition::Stale, |pending| {
                        ConnectedDisposition::Resync(Box::new(pending))
                    });
            }
            if identity.generation <= highest_identity.generation {
                return ConnectedDisposition::Stale;
            }
        }
        self.highest_identity = Some(identity);
        self.pending
            .take()
            .map_or(ConnectedDisposition::Baseline, |pending| {
                ConnectedDisposition::Resync(Box::new(pending))
            })
    }

    pub(super) fn restore_pending(&mut self, pending: PendingReconnect) {
        self.pending = Some(pending);
    }
}

impl App {
    pub(super) fn uses_supervised_app_server(&self) -> bool {
        matches!(
            self.app_server_target,
            AppServerTarget::LocalDaemon {
                canonical_projection: Some(_),
                ..
            }
        )
    }

    pub(super) fn handle_supervised_disconnect(&mut self, message: String) {
        if self.reconnect_state.is_disconnected() {
            return;
        }
        let thread_id = self.current_displayed_thread_id();
        let input_state = self.chat_widget.suspend_for_app_server_reconnect(message);
        self.reconnect_state
            .begin_disconnect(thread_id, input_state);
        self.reset_thread_event_state();
    }

    pub(super) async fn handle_supervised_connected(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        identity: AppServerInstanceIdentity,
    ) {
        let ConnectedDisposition::Resync(pending) =
            self.reconnect_state.observe_connected(identity)
        else {
            return;
        };
        let pending = *pending;
        let Some(thread_id) = pending.thread_id else {
            self.chat_widget
                .finish_app_server_reconnect_without_thread();
            return;
        };
        let resumed = app_server
            .resume_thread(
                self.config.clone(),
                thread_id,
                ResumeModelSettings::PreserveExistingThread,
            )
            .await;
        let started = match resumed {
            Err(err)
                if err
                    .chain()
                    .any(|cause| cause.to_string().contains("no rollout found for thread id")) =>
            {
                app_server
                    .start_thread_with_session_start_source(
                        &self.config,
                        /*session_start_source*/ None,
                        /*remote_cwd_override*/ None,
                    )
                    .await
            }
            result => result,
        };
        match started {
            Ok(started) => {
                if let Err(err) = self.reset_for_thread_switch(tui) {
                    self.chat_widget.add_error_message(format!(
                        "App server reconnected, but the terminal could not be refreshed: {err}"
                    ));
                    self.reconnect_state.restore_pending(pending);
                    return;
                }
                if let Err(err) = self
                    .replace_chat_widget_with_app_server_thread(
                        tui,
                        started,
                        ThreadAttachPresentation::SessionLineage,
                        /*initial_user_message*/ None,
                    )
                    .await
                {
                    self.chat_widget.add_error_message(format!(
                        "App server reconnected, but the thread could not be refreshed: {err}"
                    ));
                    self.reconnect_state.restore_pending(pending);
                    return;
                }
                self.chat_widget.restore_thread_input_state(
                    pending.input_state,
                    ThreadInputStateRestoreMode {
                        preserve_in_flight_turn: false,
                    },
                );
                self.chat_widget
                    .finish_app_server_reconnect_without_thread();
                self.chat_widget.add_info_message(
                    "Reconnected to the app server and refreshed this thread.".to_string(),
                    /*hint*/ None,
                );
            }
            Err(err) => {
                self.chat_widget.add_error_message(format!(
                    "App server reconnected, but this thread could not be resumed: {err}"
                ));
                self.reconnect_state.restore_pending(pending);
            }
        }
    }
}
