//! Exact-thread plugin command catalog, invocation, and ephemeral presentation projection.

use super::*;
use crate::app_server_session::invoke_plugin_command;
use crate::app_server_session::list_plugin_commands;
use crate::bottom_pane::slash_commands::PluginSlashCommand;
use crate::bottom_pane::slash_commands::is_builtin_command_name;
use crate::history_cell::ThreadPresentationHistoryCell;
use codex_app_server_protocol::PluginCommand;
use codex_app_server_protocol::PluginCommandInvokeResponse;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_app_server_protocol::ThreadPresentation;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;

const MAX_PRESENTATIONS: usize = 128;
const MAX_RUNTIME_SUBJECTS: usize = 128;
const MAX_CATALOG_FLIGHTS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PluginCommandCatalogSubject {
    thread_id: ThreadId,
    instance_epoch: String,
    account_slot_id: Option<String>,
    execution_generation: Option<u64>,
    cwd: PathBuf,
    invalidation_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PluginCommandRuntimeSubject {
    instance_epoch: String,
    account_slot_id: Option<String>,
    execution_generation: Option<u64>,
    stable: bool,
}

fn plugin_command_runtime_subject(
    instance_epoch: &str,
    snapshot: &SessionRuntimeSnapshot,
) -> Option<PluginCommandRuntimeSubject> {
    if matches!(
        snapshot.lifecycle.state,
        SessionRuntimeLifecycleState::NotLoaded | SessionRuntimeLifecycleState::Closing
    ) {
        return None;
    }
    let (account_slot_id, execution_generation) =
        snapshot
            .account
            .current
            .as_ref()
            .map_or((None, None), |account| {
                (
                    Some(account.account_slot_id.clone()),
                    Some(account.execution_generation),
                )
            });
    Some(PluginCommandRuntimeSubject {
        instance_epoch: instance_epoch.to_string(),
        account_slot_id,
        execution_generation,
        stable: snapshot.account.switch_state == SessionRuntimeAccountSwitchState::Stable,
    })
}

fn plugin_command_runtime_subject_for_thread(
    thread_id: ThreadId,
    account_runtime: Option<&(String, SessionRuntimeSnapshot)>,
    cached: Option<&PluginCommandRuntimeSubject>,
) -> Option<PluginCommandRuntimeSubject> {
    if let Some((instance_epoch, snapshot)) =
        account_runtime.filter(|(_, snapshot)| snapshot.thread_id == thread_id.to_string())
    {
        return plugin_command_runtime_subject(instance_epoch, snapshot);
    }
    cached.cloned()
}

#[derive(Debug)]
struct PluginCommandFlight {
    request_generation: u64,
    requested_subject: PluginCommandCatalogSubject,
    latest_subject: PluginCommandCatalogSubject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PluginCommandRequestOutcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PluginCommandCompletion {
    apply_result: bool,
    schedule_trailing: bool,
}

#[derive(Debug, Default)]
pub(super) struct PluginCommandState {
    invalidation_generation: u64,
    request_generation: u64,
    next_catalog_request_generation: u64,
    current_subject: Option<PluginCommandCatalogSubject>,
    completed_subject: Option<PluginCommandCatalogSubject>,
    flights: HashMap<ThreadId, PluginCommandFlight>,
    runtime_subjects: HashMap<ThreadId, PluginCommandRuntimeSubject>,
    commands: Vec<PluginSlashCommand>,
    presentations: HashMap<String, Arc<dyn HistoryCell>>,
    presentation_order: VecDeque<String>,
}

impl PluginCommandState {
    pub(super) fn invalidate_catalog(&mut self) {
        self.invalidation_generation = self.invalidation_generation.wrapping_add(1);
        self.commands.clear();
        self.request_generation = self.request_generation.wrapping_add(1);
        self.current_subject = None;
        self.completed_subject = None;
    }

    pub(super) fn clear_projection(&mut self) {
        self.invalidate_catalog();
        self.presentations.clear();
        self.presentation_order.clear();
    }

    pub(super) fn clear_presentations(&mut self) {
        self.presentations.clear();
        self.presentation_order.clear();
    }

    fn begin_catalog_request(&mut self, subject: PluginCommandCatalogSubject) -> Option<u64> {
        debug_assert!(
            self.current_subject
                .as_ref()
                .is_none_or(|current| current == &subject),
            "catalog subject changes must hard-invalidate the projection"
        );
        self.current_subject = Some(subject.clone());
        if self.completed_subject.as_ref() == Some(&subject) {
            return None;
        }
        if let Some(flight) = self.flights.get_mut(&subject.thread_id) {
            flight.latest_subject = subject;
            return None;
        }
        if self.flights.len() >= MAX_CATALOG_FLIGHTS {
            return None;
        }

        self.next_catalog_request_generation = self.next_catalog_request_generation.wrapping_add(1);
        let request_generation = self.next_catalog_request_generation;
        self.flights.insert(
            subject.thread_id,
            PluginCommandFlight {
                request_generation,
                requested_subject: subject.clone(),
                latest_subject: subject,
            },
        );
        Some(request_generation)
    }

    fn complete_catalog_request(
        &mut self,
        thread_id: ThreadId,
        request_generation: u64,
        outcome: PluginCommandRequestOutcome,
    ) -> PluginCommandCompletion {
        let exact_flight = self
            .flights
            .get(&thread_id)
            .is_some_and(|flight| flight.request_generation == request_generation);
        if !exact_flight {
            return PluginCommandCompletion::default();
        }

        // Clear the exact marker before checking whether its response is stale. Otherwise a
        // thread switch or subject change can strand the per-thread single-flight slot.
        let Some(flight) = self.flights.remove(&thread_id) else {
            return PluginCommandCompletion::default();
        };
        let apply_result = outcome == PluginCommandRequestOutcome::Succeeded
            && self.current_subject.as_ref() == Some(&flight.requested_subject);
        if apply_result {
            self.completed_subject = Some(flight.requested_subject.clone());
        }
        let schedule_trailing = flight.latest_subject != flight.requested_subject
            && self.current_subject.as_ref() == Some(&flight.latest_subject);
        PluginCommandCompletion {
            apply_result,
            schedule_trailing,
        }
    }

    fn observe_runtime(&mut self, instance_epoch: String, snapshot: &SessionRuntimeSnapshot) {
        let Ok(thread_id) = ThreadId::from_string(&snapshot.thread_id) else {
            return;
        };
        let Some(runtime_subject) = plugin_command_runtime_subject(&instance_epoch, snapshot)
        else {
            self.runtime_subjects.remove(&thread_id);
            return;
        };
        if !self.runtime_subjects.contains_key(&thread_id)
            && self.runtime_subjects.len() >= MAX_RUNTIME_SUBJECTS
            && let Some(evicted_thread_id) = self.runtime_subjects.keys().next().copied()
        {
            self.runtime_subjects.remove(&evicted_thread_id);
        }
        self.runtime_subjects.insert(thread_id, runtime_subject);
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

fn projected_command_name(name: &str) -> String {
    name.strip_prefix('/').unwrap_or(name).to_string()
}

fn project_commands(commands: Vec<PluginCommand>) -> Vec<PluginSlashCommand> {
    let mut short_counts = HashMap::new();
    for command in &commands {
        if let Some(short_name) = &command.short_name {
            *short_counts
                .entry(projected_command_name(short_name))
                .or_insert(0usize) += 1;
        }
    }

    commands
        .into_iter()
        .flat_map(|command| {
            let canonical = PluginSlashCommand {
                id: command.id.clone(),
                name: projected_command_name(&command.canonical_name),
                description: bounded(&command.description, 240),
                available: command.available,
                deny_reason: command
                    .deny_reason
                    .as_deref()
                    .map(|value| bounded(value, 240)),
                canonical: true,
            };
            let short = command
                .short_name
                .map(|name| projected_command_name(&name))
                .and_then(|name| {
                    (short_counts.get(&name) == Some(&1) && !is_builtin_command_name(&name)).then(
                        || PluginSlashCommand {
                            id: command.id,
                            name,
                            description: canonical.description.clone(),
                            available: canonical.available,
                            deny_reason: canonical.deny_reason.clone(),
                            canonical: false,
                        },
                    )
                });
            std::iter::once(canonical).chain(short)
        })
        .collect()
}

#[cfg(test)]
#[path = "plugin_commands_tests.rs"]
mod tests;

impl App {
    pub(super) fn invalidate_plugin_command_catalog(&mut self) {
        self.plugin_command_state.invalidate_catalog();
        self.chat_widget.set_plugin_commands(Vec::new());
    }

    pub(super) fn current_plugin_command_catalog_subject(
        &self,
    ) -> Option<PluginCommandCatalogSubject> {
        let thread_id = self.current_displayed_thread_id()?;
        let runtime = plugin_command_runtime_subject_for_thread(
            thread_id,
            self.account_runtime.as_ref(),
            self.plugin_command_state.runtime_subjects.get(&thread_id),
        )?;
        if !runtime.stable {
            return None;
        }
        Some(PluginCommandCatalogSubject {
            thread_id,
            instance_epoch: runtime.instance_epoch,
            account_slot_id: runtime.account_slot_id,
            execution_generation: runtime.execution_generation,
            cwd: self.chat_widget.config_ref().cwd.to_path_buf(),
            invalidation_generation: self.plugin_command_state.invalidation_generation,
        })
    }

    pub(super) fn observe_plugin_command_runtime(
        &mut self,
        instance_epoch: String,
        snapshot: &SessionRuntimeSnapshot,
    ) {
        self.plugin_command_state
            .observe_runtime(instance_epoch, snapshot);
    }

    pub(super) fn refresh_plugin_commands(&mut self, app_server: &AppServerSession) {
        let Some(mut subject) = self.current_plugin_command_catalog_subject() else {
            if self.current_displayed_thread_id().is_none() {
                self.plugin_command_state.clear_projection();
                self.chat_widget.set_plugin_commands(Vec::new());
            }
            return;
        };
        if self
            .plugin_command_state
            .current_subject
            .as_ref()
            .is_some_and(|current| current != &subject)
        {
            self.invalidate_plugin_command_catalog();
            let Some(refreshed_subject) = self.current_plugin_command_catalog_subject() else {
                return;
            };
            subject = refreshed_subject;
        }
        let Some(request_generation) = self
            .plugin_command_state
            .begin_catalog_request(subject.clone())
        else {
            return;
        };
        let thread_id = subject.thread_id;
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
        app_server: &AppServerSession,
        thread_id: ThreadId,
        request_generation: u64,
        result: Result<Vec<PluginCommand>, String>,
    ) {
        let completion = self.plugin_command_state.complete_catalog_request(
            thread_id,
            request_generation,
            if result.is_ok() {
                PluginCommandRequestOutcome::Succeeded
            } else {
                PluginCommandRequestOutcome::Failed
            },
        );
        let response_targets_displayed_thread =
            self.current_displayed_thread_id() == Some(thread_id);
        if completion.apply_result && !response_targets_displayed_thread {
            self.plugin_command_state.completed_subject = None;
        }
        match result {
            Ok(commands) if completion.apply_result && response_targets_displayed_thread => {
                let commands = project_commands(commands);
                self.plugin_command_state.commands = commands.clone();
                self.chat_widget.set_plugin_commands(commands);
            }
            Err(error)
                if self
                    .plugin_command_state
                    .current_subject
                    .as_ref()
                    .is_some_and(|subject| subject.thread_id == thread_id) =>
            {
                tracing::warn!(%error, "pluginCommand/list failed")
            }
            Ok(_) | Err(_) => {}
        }
        if completion.schedule_trailing {
            self.refresh_plugin_commands(app_server);
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
        if self
            .plugin_command_state
            .current_subject
            .as_ref()
            .is_none_or(|subject| subject.thread_id != thread_id)
        {
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
            || self
                .plugin_command_state
                .current_subject
                .as_ref()
                .is_none_or(|subject| subject.thread_id != thread_id)
            || self.plugin_command_state.request_generation != request_generation
        {
            return;
        }
        let title = format!("/{command_name}");
        match result {
            Ok(PluginCommandInvokeResponse::Prompt {
                prompt,
                execution_account,
            }) => {
                if let Some(execution_account) = execution_account {
                    self.chat_widget
                        .submit_plugin_prompt(prompt, execution_account);
                } else {
                    self.chat_widget.add_plugin_command_result(
                        title,
                        "Plugin prompt is missing its execution account binding.".to_string(),
                        true,
                    );
                }
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
                let body = if cleared {
                    "Goal cleared."
                } else {
                    "No goal was set."
                };
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
        let is_replacement = self.plugin_command_state.presentations.contains_key(&id);
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
        self.plugin_command_state
            .presentations
            .insert(id.clone(), cell);
        if !is_replacement {
            self.plugin_command_state.presentation_order.push_back(id);
            if self.plugin_command_state.presentation_order.len() > MAX_PRESENTATIONS
                && let Some(oldest_id) = self.plugin_command_state.presentation_order.pop_front()
                && let Some(oldest_cell) =
                    self.plugin_command_state.presentations.remove(&oldest_id)
                && let Some(index) = self
                    .transcript_cells
                    .iter()
                    .position(|candidate| Arc::ptr_eq(candidate, &oldest_cell))
            {
                self.transcript_cells.remove(index);
            }
        }
        if let Some(Overlay::Transcript(overlay)) = &mut self.overlay {
            overlay.replace_cells(self.transcript_cells.clone());
        }
        self.last_rendered_history_tail = None;
        self.backtrack_render_pending = true;
        tui.frame_requester().schedule_frame();
    }
}
