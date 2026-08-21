//! Exact-thread plugin command catalog, invocation, and ephemeral presentation projection.

use super::*;
use crate::app_server_session::invoke_plugin_command;
use crate::app_server_session::list_plugin_commands;
use crate::bottom_pane::slash_commands::PluginSlashCommand;
use crate::bottom_pane::slash_commands::is_builtin_command_name;
use crate::history_cell::ThreadPresentationHistoryCell;
use codex_app_server_protocol::PluginCommand;
use codex_app_server_protocol::PluginCommandInvokeResponse;
use codex_app_server_protocol::ThreadPresentation;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub(super) struct PluginCommandState {
    request_generation: u64,
    thread_id: Option<ThreadId>,
    commands: Vec<PluginSlashCommand>,
    presentations: HashMap<String, Arc<dyn HistoryCell>>,
}

impl PluginCommandState {
    pub(super) fn clear_projection(&mut self) {
        self.thread_id = None;
        self.commands.clear();
        self.presentations.clear();
        self.request_generation = self.request_generation.wrapping_add(1);
    }

    pub(super) fn clear_presentations(&mut self) {
        self.presentations.clear();
    }
}

fn bounded(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut result = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

fn project_commands(commands: Vec<PluginCommand>) -> Vec<PluginSlashCommand> {
    let mut short_counts = HashMap::new();
    for command in &commands {
        if let Some(short_name) = &command.short_name {
            *short_counts.entry(short_name.as_str()).or_insert(0usize) += 1;
        }
    }

    commands
        .into_iter()
        .flat_map(|command| {
            let canonical = PluginSlashCommand {
                id: command.id.clone(),
                name: command.canonical_name,
                description: bounded(&command.description, 240),
                available: command.available,
                deny_reason: command.deny_reason.as_deref().map(|value| bounded(value, 240)),
                canonical: true,
            };
            let short = command.short_name.and_then(|name| {
                (short_counts.get(name.as_str()) == Some(&1) && !is_builtin_command_name(&name))
                    .then(|| PluginSlashCommand {
                        id: command.id,
                        name,
                        description: canonical.description.clone(),
                        available: canonical.available,
                        deny_reason: canonical.deny_reason.clone(),
                        canonical: false,
                    })
            });
            std::iter::once(canonical).chain(short)
        })
        .collect()
}

impl App {
    pub(super) fn refresh_plugin_commands(&mut self, app_server: &AppServerSession) {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            self.plugin_command_state.clear_projection();
            self.chat_widget.set_plugin_commands(Vec::new());
            return;
        };
        self.plugin_command_state.request_generation = self
            .plugin_command_state
            .request_generation
            .wrapping_add(1);
        let request_generation = self.plugin_command_state.request_generation;
        self.plugin_command_state.thread_id = Some(thread_id);
        self.plugin_command_state.commands.clear();
        self.chat_widget.set_plugin_commands(Vec::new());
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = list_plugin_commands(request_handle, thread_id)
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::PluginCommandsLoaded {
                thread_id,
                request_generation,
                result,
            });
        });
    }

    pub(super) fn handle_plugin_commands_loaded(
        &mut self,
        thread_id: ThreadId,
        request_generation: u64,
        result: Result<Vec<PluginCommand>, String>,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id)
            || self.plugin_command_state.thread_id != Some(thread_id)
            || self.plugin_command_state.request_generation != request_generation
        {
            return;
        }
        match result {
            Ok(commands) => {
                let commands = project_commands(commands);
                self.plugin_command_state.commands = commands.clone();
                self.chat_widget.set_plugin_commands(commands);
            }
            Err(error) => tracing::warn!(%error, "pluginCommand/list failed"),
        }
    }

    pub(super) fn invoke_plugin_command(
        &mut self,
        app_server: &AppServerSession,
        command_id: String,
    ) {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            return;
        };
        if self.plugin_command_state.thread_id != Some(thread_id) {
            return;
        }
        let Some(command) = self
            .plugin_command_state
            .commands
            .iter()
            .find(|command| command.id == command_id && command.canonical)
        else {
            return;
        };
        if !command.available {
            self.chat_widget.add_plugin_command_result(
                format!("/{}", command.name),
                command
                    .deny_reason
                    .clone()
                    .unwrap_or_else(|| "Command unavailable".to_string()),
                true,
            );
            return;
        }
        let command_name = command.name.clone();
        let request_generation = self.plugin_command_state.request_generation;
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = invoke_plugin_command(request_handle, thread_id, command_id)
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::PluginCommandInvoked {
                thread_id,
                request_generation,
                command_name,
                result,
            });
        });
    }

    pub(super) fn handle_plugin_command_invoked(
        &mut self,
        thread_id: ThreadId,
        request_generation: u64,
        command_name: String,
        result: Result<PluginCommandInvokeResponse, String>,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id)
            || self.plugin_command_state.thread_id != Some(thread_id)
            || self.plugin_command_state.request_generation != request_generation
        {
            return;
        }
        let title = format!("/{command_name}");
        match result {
            Ok(PluginCommandInvokeResponse::Prompt { prompt }) => {
                self.chat_widget.submit_plugin_prompt(prompt);
            }
            Ok(PluginCommandInvokeResponse::McpTool { result }) => {
                let body = serde_json::to_string_pretty(&serde_json::json!({
                    "content": result.content,
                    "structuredContent": result.structured_content,
                    "isError": result.is_error,
                }))
                .unwrap_or_else(|_| "MCP tool returned an unreadable result".to_string());
                self.chat_widget.add_plugin_command_result(
                    title,
                    body,
                    result.is_error == Some(true),
                );
            }
            Ok(PluginCommandInvokeResponse::GoalGet { goal }) => {
                let body = goal.map_or_else(
                    || "No goal is set.".to_string(),
                    |goal| format!("{:?}: {}", goal.status, goal.objective),
                );
                self.chat_widget
                    .add_plugin_command_result(title, body, false);
            }
            Ok(PluginCommandInvokeResponse::GoalSet { goal }) => {
                self.chat_widget.add_plugin_command_result(
                    title,
                    format!("{:?}: {}", goal.status, goal.objective),
                    false,
                );
            }
            Ok(PluginCommandInvokeResponse::GoalClear { cleared }) => {
                let body = if cleared { "Goal cleared." } else { "No goal was set." };
                self.chat_widget
                    .add_plugin_command_result(title, body.to_string(), false);
            }
            Ok(PluginCommandInvokeResponse::Executable {
                exit_code,
                output,
                timed_out,
            }) => {
                let status = if timed_out {
                    "Timed out".to_string()
                } else {
                    exit_code.map_or_else(|| "Finished".to_string(), |code| format!("Exit {code}"))
                };
                self.chat_widget.add_plugin_command_result(
                    title,
                    format!("{status}\n{output}"),
                    timed_out || exit_code.is_some_and(|code| code != 0),
                );
            }
            Ok(PluginCommandInvokeResponse::Unavailable { deny_reason }) => {
                self.chat_widget
                    .add_plugin_command_result(title, deny_reason, true);
            }
            Err(error) => self
                .chat_widget
                .add_plugin_command_result(title, error, true),
        }
    }

    pub(super) fn upsert_thread_presentation(
        &mut self,
        tui: &mut tui::Tui,
        thread_id: ThreadId,
        item: ThreadPresentation,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        let cell = Arc::new(ThreadPresentationHistoryCell::new(item));
        let id = cell.id().to_string();
        let cell: Arc<dyn HistoryCell> = cell;
        if let Some(previous) = self.plugin_command_state.presentations.get(&id)
            && let Some(index) = self
                .transcript_cells
                .iter()
                .position(|candidate| Arc::ptr_eq(candidate, previous))
        {
            self.transcript_cells[index] = cell.clone();
        } else {
            self.transcript_cells.push(cell.clone());
        }
        self.plugin_command_state.presentations.insert(id, cell);
        if let Some(Overlay::Transcript(overlay)) = &mut self.overlay {
            overlay.replace_cells(self.transcript_cells.clone());
        }
        self.last_rendered_history_tail = None;
        self.backtrack_render_pending = true;
        tui.frame_requester().schedule_frame();
    }
}
