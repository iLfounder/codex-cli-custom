//! Local working-directory changes that preserve conversation history.

use super::session_lifecycle::ThreadAttachPresentation;
use super::*;
use crate::app_server_session::ForkGoalContinuation::DeferUntilNextTurn;
use crate::cli::is_exact_tilde_home_form;
use crate::history_cell::McpInventoryLoadingCell as LoadingCell;
use codex_app_server_protocol::ThreadBackgroundTerminalsListParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsListResponse as ListResponse;
use codex_utils_path_uri::LegacyAppPathString;

fn resolve_remote_working_directory(
    current_cwd: &LegacyAppPathString,
    requested_cwd: &str,
) -> Result<LegacyAppPathString, String> {
    let requested_cwd = if requested_cwd.is_empty() {
        "~"
    } else {
        requested_cwd
    };
    if is_exact_tilde_home_form(requested_cwd) {
        return Ok(LegacyAppPathString::from_string(requested_cwd));
    }
    let base = current_cwd.to_inferred_path_uri().ok_or_else(|| {
        format!(
            "Cannot resolve a path relative to {}.",
            current_cwd.render_for_ui()
        )
    })?;
    base.join(requested_cwd)
        .map(LegacyAppPathString::from)
        .map_err(|error| format!("Cannot resolve remote directory {requested_cwd}: {error}"))
}

impl App {
    fn working_directory_error(&mut self, message: impl Into<String>) {
        self.chat_widget.add_error_message(message.into());
    }

    pub(super) async fn change_remote_working_directory(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        requested_cwd: &str,
    ) {
        let Some(session) = self.primary_session_configured.as_ref() else {
            return self.working_directory_error("The remote session is not configured.");
        };
        let Some(current_cwd) = session.remote_cwd() else {
            return self.working_directory_error("The active session is not remote.");
        };
        let Some(thread_id) = self.chat_widget.thread_id() else {
            return;
        };
        let requested_cwd = if requested_cwd.is_empty() {
            "~"
        } else {
            requested_cwd
        };
        let target = match resolve_remote_working_directory(current_cwd, requested_cwd) {
            Ok(target) => target,
            Err(error) => return self.working_directory_error(error),
        };
        let has_rollout = session.remote_rollout_path().is_some();

        let agents = self.agent_navigation.ordered_threads();
        if agents
            .iter()
            .any(|(id, agent)| *id != thread_id && !agent.is_closed && agent.is_running)
        {
            return self.working_directory_error("Cannot change: another agent is running.");
        }
        for tracked_id in agents
            .iter()
            .filter_map(|(id, agent)| (!agent.is_closed).then_some(*id))
            .chain(std::iter::once(thread_id))
        {
            let request = ClientRequest::ThreadBackgroundTerminalsList {
                request_id: app_server.next_request_id(),
                params: ThreadBackgroundTerminalsListParams {
                    thread_id: tracked_id.to_string(),
                    cursor: None,
                    limit: Some(1),
                },
            };
            let result = app_server
                .request_handle()
                .request_typed::<ListResponse>(request)
                .await;
            if !matches!(result, Ok(response) if response.data.is_empty()) {
                return self.working_directory_error("Active background terminals block /cd.");
            }
        }

        let transitioned = if has_rollout {
            app_server
                .fork_thread_at_remote_cwd(self.config.clone(), thread_id, &target)
                .await
        } else {
            app_server
                .start_thread_with_session_start_source(
                    &self.config,
                    /*session_start_source*/ None,
                    Some(&target),
                )
                .await
        };
        let transitioned = match transitioned {
            Ok(value) => value,
            Err(error) => {
                return self.working_directory_error(format!("Failed to change: {error}"));
            }
        };
        let returned_cwd = transitioned.session.remote_cwd();
        let cwd_matches = if is_exact_tilde_home_form(requested_cwd) {
            returned_cwd.is_some_and(|cwd| cwd.to_inferred_path_uri().is_some())
        } else {
            returned_cwd == Some(&target)
        };
        if transitioned.session.thread_id == thread_id || !cwd_matches {
            let replacement_id = transitioned.session.thread_id;
            if replacement_id != thread_id {
                let _ = app_server.thread_unsubscribe(replacement_id).await;
                if has_rollout {
                    let _ = app_server.thread_archive(replacement_id).await;
                }
            }
            return self.working_directory_error("Requested remote directory was not applied.");
        }
        if let Err(error) = app_server.thread_unsubscribe(thread_id).await {
            let replacement_id = transitioned.session.thread_id;
            let _ = app_server.thread_unsubscribe(replacement_id).await;
            if has_rollout {
                let _ = app_server.thread_archive(replacement_id).await;
            }
            return self.working_directory_error(format!("Cannot change directories: {error}"));
        }

        let display_cwd = transitioned.session.display_cwd();
        let attach = App::replace_chat_widget_with_app_server_thread;
        if let Err(error) = attach(
            self,
            tui,
            transitioned,
            ThreadAttachPresentation::SessionLineage,
            /*message*/ None,
        )
        .await
        {
            return self.working_directory_error(format!("Could not restore session: {error}"));
        }
        self.cancel_pending_key_chord();
        self.chat_widget.add_info_message(
            format!("Working directory changed to: {display_cwd}"),
            /*hint*/ None,
        );
        tui.frame_requester().schedule_frame();
    }

    pub(super) async fn change_working_directory(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        cwd: AbsolutePathBuf,
    ) {
        if self.config.ephemeral || !cwd.as_path().is_dir() {
            return self.working_directory_error("This task cannot be safely replaced.");
        }
        let Some(thread_id) = self.chat_widget.thread_id() else {
            return;
        };
        let cells = &self.transcript_cells;
        if cells.iter().any(|cell| cell.as_any().is::<LoadingCell>()) {
            return self.working_directory_error("MCP inventory is still loading.");
        }
        let agents = self.agent_navigation.ordered_threads();
        let closed_agents: HashSet<_> = agents
            .iter()
            .filter_map(|(id, agent)| agent.is_closed.then_some(*id))
            .collect();
        let active = self.thread_event_channels.iter().any(|(id, channel)| {
            *id != thread_id
                && !closed_agents.contains(id)
                && !channel
                    .store
                    .try_lock()
                    .is_ok_and(|store| store.active_turn_id().is_none())
        });
        if active || agents.iter().any(|(t, a)| *t != thread_id && a.is_running) {
            return self.working_directory_error("Cannot change: another agent is running.");
        }
        let open_agents: Vec<_> = agents
            .iter()
            .filter_map(|(id, agent)| (!agent.is_closed).then_some(*id))
            .collect();
        let mut config = match self.rebuild_config_for_cwd(cwd.to_path_buf()).await {
            Ok(config) => config,
            Err(err) => return self.working_directory_error(format!("Cannot load {cwd:?}: {err}")),
        };
        if config.active_project.trust_level.is_none() {
            return self.working_directory_error("This directory is not trusted; run Codex there.");
        }
        if let Some(profile) = self.runtime_permission_profile_override.as_ref()
            && profile.active_permission_profile.is_some()
            && (!profile.matches_config(&config)
                || config.permissions.profile_workspace_roots()
                    != self.config.permissions.profile_workspace_roots())
        {
            return self.working_directory_error("Permission profile has different settings.");
        }
        if let Some(profile) = self.runtime_permission_profile_override.as_ref()
            && profile.turn_override == RuntimePermissionProfileTurnOverride::Preserve
            && profile.active_permission_profile.is_none()
            && !crate::app_server_session::permission_profile_is_safely_represented_by_sandbox_mode(
                &profile.permission_profile,
                cwd.as_path(),
            )
        {
            return self.working_directory_error("Permission profile cannot be preserved by /cd.");
        }
        self.apply_runtime_policy_overrides(&mut config, RuntimePolicyOverrideScope::All);
        if self.runtime_permission_profile_override.is_some() {
            let reviewer = self.config.approvals_reviewer;
            let reviewers = &config.config_layer_stack.requirements().approvals_reviewer;
            if let Err(error) = reviewers.can_set(&reviewer) {
                return self.working_directory_error(format!("Approvals reviewer: {error}"));
            }
            config.approvals_reviewer = reviewer;
        }
        let actual = config.permissions.approval_policy.value();
        let approval = self
            .runtime_approval_policy_override
            .map(RuntimeApprovalPolicyOverride::policy);
        let profile = self.runtime_permission_profile_override.as_ref();
        if approval.is_some_and(|p| actual != p.to_core())
            || profile.is_some_and(|profile| !profile.matches_config(&config))
        {
            return;
        }
        let keymap = match RuntimeKeymap::from_config(&config.tui_keymap) {
            Ok(keymap) => keymap,
            Err(error) => return self.chat_widget.add_error_message(error),
        };
        config.service_tier = self.chat_widget.configured_service_tier();
        let rollout = self.chat_widget.rollout_path();
        let has_rollout = rollout.as_deref().is_some_and(rollout_path_is_resumable);
        let channels = &self.thread_event_channels;
        if !has_rollout
            && (!self.chat_widget.token_usage().is_zero()
                || channels.get(&thread_id).is_some_and(|channel| {
                    channel.store.try_lock().map_or(/*default*/ true, |store| {
                        !store.turns.is_empty()
                            || store.buffer.iter().any(|event| {
                                matches!(
                                    event,
                                    ThreadBufferedEvent::Notification(notification)
                                        if matches!(notification.as_ref(),
                                            ServerNotification::TurnStarted(_)
                                            | ServerNotification::TurnCompleted(_))
                                )
                            })
                    })
                }))
        {
            return self.working_directory_error("Conversation history is not saved.");
        }
        let mut ids: HashSet<_> = channels
            .keys()
            .copied()
            .filter(|id| !closed_agents.contains(id))
            .collect();
        ids.extend(open_agents);
        let descendants = ids.iter().copied().filter(|id| *id != thread_id);
        for tracked_id in std::iter::once(thread_id).chain(descendants) {
            let request = ClientRequest::ThreadBackgroundTerminalsList {
                request_id: app_server.next_request_id(),
                params: ThreadBackgroundTerminalsListParams {
                    thread_id: tracked_id.to_string(),
                    cursor: None,
                    limit: Some(1),
                },
            };
            let handle = app_server.request_handle();
            let result = handle.request_typed::<ListResponse>(request).await;
            if !matches!(result, Ok(response) if response.data.is_empty()) {
                return self.working_directory_error("Active background terminals block /cd.");
            }
        }
        let transitioned = if has_rollout {
            app_server
                .fork_thread_at(
                    config.clone(),
                    thread_id,
                    /*last_turn_id*/ None,
                    /*before_turn_id*/ None,
                    DeferUntilNextTurn,
                )
                .await
        } else {
            app_server
                .start_thread_with_session_start_source(
                    &config, /*session_start_source*/ None, /*remote_cwd_override*/ None,
                )
                .await
        };
        let transitioned = match transitioned {
            Ok(value) => value,
            Err(e) => return self.working_directory_error(format!("Failed to change: {e}")),
        };
        let session = &transitioned.session;
        if session.thread_id == thread_id
            || session.native_cwd().is_none_or(|session_cwd| {
                crate::session_resume::cwds_differ(session_cwd.as_path(), cwd.as_path())
            })
            || session.native_runtime_workspace_roots() != Some(config.workspace_roots.as_slice())
            || session.approval_policy.to_core() != config.permissions.approval_policy.value()
            || session.approvals_reviewer != config.approvals_reviewer
            || session.active_permission_profile != config.permissions.active_permission_profile()
        {
            if session.thread_id != thread_id {
                let _ = app_server.thread_unsubscribe(session.thread_id).await;
                if has_rollout {
                    let _ = app_server.thread_archive(session.thread_id).await;
                }
            }
            return self.working_directory_error("Requested directory or permissions not applied.");
        }
        if let Err(error) = app_server.thread_unsubscribe(thread_id).await {
            let replacement_id = transitioned.session.thread_id;
            let _ = app_server.thread_unsubscribe(replacement_id).await;
            if has_rollout {
                let _ = app_server.thread_archive(replacement_id).await;
            }
            return self.working_directory_error(format!("Cannot change directories: {error}"));
        }
        for tracked_id in ids.into_iter().filter(|id| *id != thread_id) {
            if let Err(error) = app_server.thread_unsubscribe(tracked_id).await {
                tracing::warn!("failed to unsubscribe tracked thread {tracked_id}: {error}");
            }
        }
        self.config = config;
        self.file_search
            .update_search_dir(self.config.cwd.to_path_buf());
        let notify = &self.config.tui_notifications;
        tui.set_notification_settings(notify.method, notify.condition);
        if let Err(error) = tui.clear_ambient_pet_image() {
            tracing::warn!(%error, "failed to clear ambient pet image");
        }
        let attach = App::replace_chat_widget_with_app_server_thread;
        let (lineage, message) = (ThreadAttachPresentation::SessionLineage, None);
        if let Err(error) = attach(self, tui, transitioned, lineage, message).await {
            return self.working_directory_error(format!("Could not restore session: {error}"));
        }
        self.cancel_pending_key_chord();
        self.keymap = keymap;
        self.restore_runtime_theme_from_config();
        self.runtime_working_directory_override = Some(cwd.to_path_buf());
        emit_project_config_warnings(&self.app_event_tx, &self.config);
        let message = format!("Working directory changed to: {}", cwd.display());
        self.chat_widget.add_info_message(message, /*hint*/ None);
        if !self.config.bypass_hook_trust {
            let load_review = crate::startup_hooks_review::load_startup_hooks_review_entry;
            let hooks = load_review(
                app_server.request_handle(),
                cwd.to_path_buf(),
                /*skip_server_hook_scan*/ false,
            )
            .await;
            if hooks.hooks.iter().any(crate::hooks_rpc::hook_needs_review) {
                self.chat_widget.open_hooks_browser(hooks);
            }
        }
        tui.frame_requester().schedule_frame();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_cwd_resolution_preserves_tilde_for_the_server() {
        let cwd = LegacyAppPathString::from_string("/Users/daniel/work/repo");

        assert_eq!(
            resolve_remote_working_directory(&cwd, "~").expect("tilde cwd"),
            LegacyAppPathString::from_string("~")
        );
        assert_eq!(
            resolve_remote_working_directory(&cwd, "~/src").expect("tilde child cwd"),
            LegacyAppPathString::from_string("~/src")
        );
    }

    #[test]
    fn remote_cwd_resolution_uses_the_execution_hosts_path_convention() {
        let cwd = LegacyAppPathString::from_string("/Users/daniel/work/repo");

        assert_eq!(
            resolve_remote_working_directory(&cwd, "../other").expect("relative remote cwd"),
            LegacyAppPathString::from_string("/Users/daniel/work/other")
        );
    }

    #[test]
    fn remote_cwd_resolution_treats_non_exact_tilde_forms_as_relative() {
        let cwd = LegacyAppPathString::from_string("/Users/daniel/work/repo");

        assert_eq!(
            resolve_remote_working_directory(&cwd, "~//other").expect("double-slash cwd"),
            LegacyAppPathString::from_string("/Users/daniel/work/repo/~/other")
        );
        assert_eq!(
            resolve_remote_working_directory(&cwd, r"~/\other").expect("mixed-slash cwd"),
            LegacyAppPathString::from_string(r"/Users/daniel/work/repo/~/\other")
        );
    }
}
