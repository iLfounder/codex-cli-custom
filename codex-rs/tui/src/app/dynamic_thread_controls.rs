//! Typed model-requested TUI controls with deferred presentation transitions.

use super::*;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DynamicThreadControl {
    Clear,
    New,
}

#[derive(Debug)]
pub(super) struct PendingDynamicThreadControl {
    thread_id: ThreadId,
    turn_id: String,
    call_id: String,
    tool: String,
    control: DynamicThreadControl,
}

impl App {
    pub(super) async fn handle_dynamic_thread_control_request(
        &mut self,
        app_server: &AppServerSession,
        request: &ServerRequest,
    ) -> bool {
        let ServerRequest::DynamicToolCall { request_id, params } = request else {
            return false;
        };
        let control = match (params.namespace.as_deref(), params.tool.as_str()) {
            (None, "threadClear") => DynamicThreadControl::Clear,
            (None, "threadNew") => DynamicThreadControl::New,
            _ => return false,
        };
        let parsed_thread_id = ThreadId::from_string(&params.thread_id).ok();
        let active_turn_id = match parsed_thread_id {
            Some(thread_id) => self.active_turn_id_for_thread(thread_id).await,
            None => None,
        };
        let valid_target = parsed_thread_id.is_some_and(|thread_id| {
            self.primary_thread_id == Some(thread_id)
                && self.current_displayed_thread_id() == Some(thread_id)
        }) && active_turn_id.as_deref() == Some(params.turn_id.as_str());
        let empty_arguments = params
            .arguments
            .as_object()
            .is_some_and(serde_json::Map::is_empty);
        if !valid_target || !empty_arguments || self.pending_dynamic_thread_control.is_some() {
            let message = if self.pending_dynamic_thread_control.is_some() {
                "Another typed thread control is already pending."
            } else if !empty_arguments {
                "Typed thread controls accept an empty object only."
            } else {
                "Typed thread controls target only the active primary turn."
            };
            if let Err(error) = self
                .reject_app_server_request(app_server, request_id.clone(), message.to_string())
                .await
            {
                tracing::warn!(%error, "failed to reject typed thread control");
            }
            return true;
        }
        let Some(thread_id) = parsed_thread_id else {
            return true;
        };
        self.pending_dynamic_thread_control = Some(PendingDynamicThreadControl {
            thread_id,
            turn_id: params.turn_id.clone(),
            call_id: params.call_id.clone(),
            tool: params.tool.clone(),
            control,
        });
        let response = DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: match control {
                    DynamicThreadControl::Clear => {
                        "The thread will clear after this tool item completes."
                    }
                    DynamicThreadControl::New => {
                        "A new thread will open after this tool item completes."
                    }
                }
                .to_string(),
            }],
            success: true,
        };
        let result = match serde_json::to_value(response) {
            Ok(value) => {
                app_server
                    .resolve_server_request(request_id.clone(), value)
                    .await
            }
            Err(error) => Err(std::io::Error::other(error)),
        };
        if let Err(error) = result {
            self.pending_dynamic_thread_control = None;
            tracing::warn!(%error, "failed to resolve typed thread control");
        }
        true
    }

    pub(super) fn handle_dynamic_thread_control_completed(
        &mut self,
        notification: &ItemCompletedNotification,
    ) -> bool {
        let Some(pending) = self.pending_dynamic_thread_control.as_ref() else {
            return false;
        };
        let ThreadItem::DynamicToolCall {
            id,
            namespace,
            tool,
            status,
            success,
            ..
        } = &notification.item
        else {
            return false;
        };
        if notification.thread_id != pending.thread_id.to_string()
            || notification.turn_id != pending.turn_id
            || id != &pending.call_id
            || namespace.is_some()
            || tool != &pending.tool
        {
            return false;
        }
        let successful = *status == DynamicToolCallStatus::Completed
            && *success == Some(true)
            && self.primary_thread_id == Some(pending.thread_id)
            && self.current_displayed_thread_id() == Some(pending.thread_id);
        let control = pending.control;
        self.pending_dynamic_thread_control = None;
        if successful {
            match control {
                DynamicThreadControl::Clear => {
                    self.app_event_tx.send(AppEvent::ClearUi { name: None })
                }
                DynamicThreadControl::New => {
                    self.app_event_tx.send(AppEvent::NewSession { name: None })
                }
            }
        }
        true
    }
}

#[cfg(test)]
#[path = "dynamic_thread_controls_tests.rs"]
mod tests;
