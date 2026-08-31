//! Canonical app-server reconnect and thread projection resynchronization.

use super::App;
use crate::AppServerTarget;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::ResumeModelSettings;
use crate::chatwidget::ThreadInputState;
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

    pub(super) fn handle_supervised_disconnect(&mut self, _message: String) {
        // The supervisor owns transport recovery. Use the same offline input and event
        // quarantine as generic remote recovery, without resetting cached thread state.
        self.begin_reconnect();
    }

    pub(super) async fn handle_supervised_connected(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        app_event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::app_event::AppEvent>,
        identity: AppServerInstanceIdentity,
    ) {
        let ConnectedDisposition::Resync(pending) =
            self.reconnect_state.observe_connected(identity)
        else {
            return;
        };
        let mut pending = *pending;
        let hydrated = async {
            let bootstrap = app_server.bootstrap(&self.config).await?;
            let thread = if let Some(thread_id) = pending.thread_id {
                let resumed = app_server
                    .resume_thread(
                        self.config.clone(),
                        thread_id,
                        ResumeModelSettings::PreserveExistingThread,
                    )
                    .await;
                let started = match resumed {
                    Err(error)
                        if matches!(
                            error.downcast_ref::<codex_app_server_client::TypedRequestError>(),
                            Some(codex_app_server_client::TypedRequestError::Server { source, .. })
                                if source.code == -32600
                                    && source.message.contains("no rollout found for thread id")
                        ) =>
                    {
                        // A canonical thread that never materialized can be replaced, but its
                        // pending input is never submitted as part of the replacement.
                        app_server
                            .start_thread_with_session_start_source(
                                &self.config,
                                /*session_start_source*/ None,
                                /*remote_cwd_override*/ None,
                            )
                            .await?
                    }
                    result => result?,
                };
                Some(started)
            } else {
                None
            };
            Ok::<_, color_eyre::Report>((bootstrap, thread))
        }
        .await;
        let result = match hydrated {
            Ok((bootstrap, thread)) => {
                // Prefer the live offline draft, which may have been edited since disconnect.
                if self.chat_widget.capture_thread_input_state().is_none() {
                    self.chat_widget
                        .restore_reconnected_input(pending.input_state.take());
                }
                self.finish_reconnect_projection(
                    tui,
                    app_server,
                    app_event_rx,
                    bootstrap,
                    thread,
                )
                .await
            }
            Err(error) => Err(error),
        };
        if result.is_err() {
            // Transport errors may contain endpoint credentials; keep the notice generic.
            self.chat_widget.add_error_message(
                "App server reconnected, but this thread could not be refreshed. Input remains paused."
                    .to_string(),
            );
            self.reconnect_state.restore_pending(pending);
        }
    }
}
