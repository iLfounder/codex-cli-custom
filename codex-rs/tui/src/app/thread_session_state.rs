use super::App;
use crate::session_resume::read_session_model;
use crate::session_state::ThreadSessionState;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::Thread;
use codex_protocol::ThreadId;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::PermissionProfile;
use codex_utils_path_uri::LegacyAppPathString;

fn legacy_api_path(path: &impl serde::Serialize) -> LegacyAppPathString {
    let value = serde_json::to_value(path).expect("app-server paths must serialize");
    LegacyAppPathString::from_string(
        value
            .as_str()
            .expect("app-server paths serialize as strings")
            .to_string(),
    )
}

impl App {
    pub(super) async fn update_cached_thread_name(
        &mut self,
        thread_id: ThreadId,
        thread_name: Option<String>,
    ) {
        if let Some(session) = self.primary_session_configured.as_mut()
            && session.thread_id == thread_id
        {
            session.thread_name = thread_name.clone();
        }

        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            let mut store = channel.store.lock().await;
            if let Some(session) = store.session.as_mut() {
                session.thread_name = thread_name;
            }
        }
    }

    pub(super) async fn sync_active_thread_service_tier_to_cached_session(&mut self) {
        let Some(active_thread_id) = self.active_thread_id else {
            return;
        };

        let service_tier = self.chat_widget.current_service_tier().map(str::to_string);
        let update_session = |session: &mut ThreadSessionState| {
            session.service_tier = service_tier.clone();
        };

        if self.primary_thread_id == Some(active_thread_id)
            && let Some(session) = self.primary_session_configured.as_mut()
        {
            update_session(session);
        }

        if let Some(channel) = self.thread_event_channels.get(&active_thread_id) {
            let mut store = channel.store.lock().await;
            if let Some(session) = store.session.as_mut() {
                update_session(session);
            }
        }
    }

    pub(super) async fn sync_active_thread_permission_settings_to_cached_session(&mut self) {
        let Some(active_thread_id) = self.active_thread_id else {
            return;
        };

        let approval_policy = AskForApproval::from(self.config.permissions.approval_policy.value());
        let approvals_reviewer = self.config.approvals_reviewer;
        let permission_profile = self
            .chat_widget
            .config_ref()
            .permissions
            .permission_profile()
            .clone();
        let active_permission_profile = self
            .chat_widget
            .config_ref()
            .permissions
            .active_permission_profile();
        let update_session = |session: &mut ThreadSessionState| {
            session.approval_policy = approval_policy;
            session.approvals_reviewer = approvals_reviewer;
            session.set_native_permission_profile(permission_profile.clone());
            session.active_permission_profile = active_permission_profile.clone();
        };

        if self.primary_thread_id == Some(active_thread_id)
            && let Some(session) = self.primary_session_configured.as_mut()
        {
            update_session(session);
        }

        if let Some(channel) = self.thread_event_channels.get(&active_thread_id) {
            let mut store = channel.store.lock().await;
            if let Some(session) = store.session.as_mut() {
                update_session(session);
            }
        }
    }

    pub(super) async fn session_state_for_thread_read(
        &self,
        thread_id: ThreadId,
        thread: &Thread,
    ) -> ThreadSessionState {
        let permission_profile = self.current_permission_profile();
        let active_permission_profile = self.current_active_permission_profile();
        let uses_remote_workspace = self.app_server_target.uses_remote_workspace();
        let mut session = if let Some(mut session) = self.primary_session_configured.clone() {
            if session.thread_id != thread_id {
                // `thread/read` does not include thread settings, so do not carry
                // thread-scoped state from the currently active session.
                session.collaboration_mode = None;
                session.personality = None;
            }
            session
        } else {
            let execution_context = if uses_remote_workspace {
                crate::session_state::SessionExecutionContext::Remote {
                    cwd: legacy_api_path(&thread.cwd),
                    runtime_workspace_roots: Vec::new(),
                    sandbox: codex_app_server_protocol::SandboxPolicy::ReadOnly {
                        network_access: false,
                    },
                    rollout_path: thread.path.as_ref().map(legacy_api_path),
                }
            } else {
                crate::session_state::SessionExecutionContext::native(
                    legacy_api_path(&thread.cwd)
                        .to_inferred_abs_path()
                        .expect("local app-server must return a native cwd"),
                    self.config.workspace_roots.clone(),
                    permission_profile.clone(),
                    thread.path.as_ref().map(|path| {
                        legacy_api_path(path)
                            .to_inferred_abs_path()
                            .expect("local app-server must return a native rollout path")
                            .into_path_buf()
                    }),
                )
            };
            ThreadSessionState {
                thread_id,
                forked_from_id: None,
                fork_parent_title: None,
                thread_name: None,
                model: self.chat_widget.current_model().to_string(),
                model_provider_id: self.config.model_provider_id.clone(),
                service_tier: self.chat_widget.current_service_tier().map(str::to_string),
                approval_policy: AskForApproval::from(
                    self.config.permissions.approval_policy.value(),
                ),
                approvals_reviewer: self.config.approvals_reviewer,
                active_permission_profile: active_permission_profile.clone(),
                execution_context,
                instruction_source_paths: Vec::new(),
                reasoning_effort: self.chat_widget.current_reasoning_effort(),
                collaboration_mode: None,
                personality: None,
                message_history: None,
                network_proxy: None,
            }
        };
        session.thread_id = thread_id;
        session.thread_name = thread.name.clone();
        session.model_provider_id = thread.model_provider.clone();
        if uses_remote_workspace {
            session.set_remote_cwd_and_rollout(
                legacy_api_path(&thread.cwd),
                thread.path.as_ref().map(legacy_api_path),
            );
        } else {
            session.set_cwd_retargeting_implicit_runtime_workspace_root(
                legacy_api_path(&thread.cwd)
                    .to_inferred_abs_path()
                    .expect("local app-server must return a native cwd"),
            );
            session.set_native_permission_profile(permission_profile);
            session.set_native_rollout_path(thread.path.as_ref().map(|path| {
                legacy_api_path(path)
                    .to_inferred_abs_path()
                    .expect("local app-server must return a native rollout path")
                    .into_path_buf()
            }));
        }
        session.active_permission_profile = active_permission_profile;
        session.instruction_source_paths = Vec::new();
        if !uses_remote_workspace
            && let Some(model) = read_session_model(
                self.state_db.as_deref(),
                thread_id,
                session.native_rollout_path(),
            )
            .await
        {
            session.model = model;
        } else if !uses_remote_workspace && thread.path.is_some() {
            session.model.clear();
        }
        session.message_history = None;
        session
    }

    fn current_permission_profile(&self) -> PermissionProfile {
        self.chat_widget
            .config_ref()
            .permissions
            .permission_profile()
            .clone()
    }

    fn current_active_permission_profile(&self) -> Option<ActivePermissionProfile> {
        self.chat_widget
            .config_ref()
            .permissions
            .active_permission_profile()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::side::SideThreadState;
    use crate::app::test_support::make_test_app;
    use crate::app::thread_events::ThreadEventChannel;
    use crate::legacy_core::config::PermissionProfileSnapshot;
    use crate::test_support::PathBufExt;
    use crate::test_support::test_path_buf;
    use codex_app_server_protocol::AskForApproval;
    use codex_config::types::ApprovalsReviewer;
    use codex_protocol::config_types::ServiceTier;
    use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
    use codex_protocol::models::ManagedFileSystemPermissions;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;
    use codex_protocol::permissions::FileSystemSandboxEntry;
    use codex_protocol::permissions::FileSystemSpecialPath;
    use codex_protocol::permissions::NetworkSandboxPolicy;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn test_thread_session(thread_id: ThreadId, cwd: PathBuf) -> ThreadSessionState {
        ThreadSessionState {
            thread_id,
            forked_from_id: None,
            fork_parent_title: None,
            thread_name: None,
            model: "gpt-test".to_string(),
            model_provider_id: "test-provider".to_string(),
            service_tier: None,
            approval_policy: AskForApproval::Never,
            approvals_reviewer: ApprovalsReviewer::User,
            execution_context: crate::session_state::SessionExecutionContext::native(
                cwd.abs(),
                vec![cwd.abs()],
                PermissionProfile::read_only(),
                Some(PathBuf::new()),
            ),
            active_permission_profile: None,
            instruction_source_paths: Vec::new(),
            reasoning_effort: None,
            collaboration_mode: None,
            personality: None,
            message_history: None,
            network_proxy: None,
        }
    }

    #[tokio::test]
    async fn accepted_thread_name_updates_primary_and_event_cache_together() {
        let mut app = make_test_app().await;
        let thread_id = ThreadId::new();
        let session = test_thread_session(thread_id, test_path_buf("/tmp/main"));
        app.primary_session_configured = Some(session.clone());
        app.thread_event_channels.insert(
            thread_id,
            ThreadEventChannel::new_with_session(/*capacity*/ 4, session, Vec::new()),
        );

        app.update_cached_thread_name(thread_id, Some("Renamed thread".to_string()))
            .await;

        assert_eq!(
            app.primary_session_configured
                .as_ref()
                .and_then(|session| session.thread_name.as_deref()),
            Some("Renamed thread")
        );
        let store = app
            .thread_event_channels
            .get(&thread_id)
            .expect("thread channel")
            .store
            .lock()
            .await;
        assert_eq!(
            store
                .session
                .as_ref()
                .and_then(|session| session.thread_name.as_deref()),
            Some("Renamed thread")
        );
    }

    #[tokio::test]
    async fn footer_name_sync_uses_primary_then_side_session_cache() {
        let mut app = make_test_app().await;
        let primary_thread_id = ThreadId::new();
        let side_thread_id = ThreadId::new();
        let mut primary_session =
            test_thread_session(primary_thread_id, test_path_buf("/tmp/main"));
        primary_session.thread_name = Some("Cached primary".to_string());
        app.primary_thread_id = Some(primary_thread_id);
        app.active_thread_id = Some(primary_thread_id);
        app.primary_session_configured = Some(primary_session.clone());

        let mut stale_primary_widget = primary_session;
        stale_primary_widget.thread_name = Some("Stale widget primary".to_string());
        app.chat_widget.handle_thread_session(stale_primary_widget);
        app.sync_footer_runtime_projection_for_thread(primary_thread_id);
        assert_eq!(
            app.chat_widget.thread_name().as_deref(),
            Some("Cached primary")
        );

        let mut side_session = test_thread_session(side_thread_id, test_path_buf("/tmp/side"));
        side_session.thread_name = Some("Cached side".to_string());
        app.thread_event_channels.insert(
            side_thread_id,
            ThreadEventChannel::new_with_session(
                /*capacity*/ 4,
                side_session.clone(),
                Vec::new(),
            ),
        );
        app.active_thread_id = Some(side_thread_id);
        let mut stale_side_widget = side_session;
        stale_side_widget.thread_name = Some("Stale widget side".to_string());
        app.chat_widget.handle_thread_session(stale_side_widget);
        app.sync_footer_runtime_projection_for_thread(side_thread_id);
        assert_eq!(
            app.chat_widget.thread_name().as_deref(),
            Some("Cached side")
        );
    }

    #[tokio::test]
    async fn permission_settings_sync_updates_active_snapshot_without_rewriting_side_thread() {
        let mut app = make_test_app().await;
        let main_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000401").expect("valid thread");
        let side_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000402").expect("valid thread");
        let main_session = test_thread_session(main_thread_id, test_path_buf("/tmp/main"));
        let side_session = ThreadSessionState {
            approval_policy: AskForApproval::OnRequest,
            ..test_thread_session(side_thread_id, test_path_buf("/tmp/side"))
                .with_native_permission_profile(PermissionProfile::workspace_write())
        };

        app.primary_thread_id = Some(main_thread_id);
        app.active_thread_id = Some(main_thread_id);
        app.primary_session_configured = Some(main_session.clone());
        app.thread_event_channels.insert(
            main_thread_id,
            ThreadEventChannel::new_with_session(
                /*capacity*/ 4,
                main_session.clone(),
                Vec::new(),
            ),
        );
        app.thread_event_channels.insert(
            side_thread_id,
            ThreadEventChannel::new_with_session(
                /*capacity*/ 4,
                side_session.clone(),
                Vec::new(),
            ),
        );
        app.side_threads
            .insert(side_thread_id, SideThreadState::new(main_thread_id));
        app.config.permissions.approval_policy =
            codex_config::Constrained::allow_any(AskForApproval::OnRequest.to_core());
        app.config.approvals_reviewer = ApprovalsReviewer::AutoReview;
        let expected_permission_profile = PermissionProfile::workspace_write();
        let expected_active_permission_profile =
            ActivePermissionProfile::new(BUILT_IN_PERMISSION_PROFILE_WORKSPACE);
        app.chat_widget.handle_thread_session(main_session.clone());
        app.chat_widget
            .set_permission_profile_from_session_snapshot(PermissionProfileSnapshot::active(
                expected_permission_profile.clone(),
                expected_active_permission_profile.clone(),
            ))
            .expect("set widget permission profile");
        app.config
            .permissions
            .set_permission_profile(expected_permission_profile.clone())
            .expect("set permission profile");

        app.sync_active_thread_permission_settings_to_cached_session()
            .await;

        let expected_main_session = ThreadSessionState {
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: ApprovalsReviewer::AutoReview,
            active_permission_profile: Some(expected_active_permission_profile),
            ..main_session.with_native_permission_profile(expected_permission_profile)
        };
        assert_eq!(
            app.primary_session_configured,
            Some(expected_main_session.clone())
        );

        let main_store_session = app
            .thread_event_channels
            .get(&main_thread_id)
            .expect("main thread channel")
            .store
            .lock()
            .await
            .session
            .clone();
        assert_eq!(main_store_session, Some(expected_main_session));

        let side_store_session = app
            .thread_event_channels
            .get(&side_thread_id)
            .expect("side thread channel")
            .store
            .lock()
            .await
            .session
            .clone();
        assert_eq!(side_store_session, Some(side_session));
    }

    #[tokio::test]
    async fn permission_settings_sync_preserves_active_profile_only_rules() {
        let mut app = make_test_app().await;
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000403").expect("valid thread");
        let profile: PermissionProfile = PermissionProfile::Managed {
            network: NetworkSandboxPolicy::Restricted,
            file_system: ManagedFileSystemPermissions::Restricted {
                entries: vec![
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::Root,
                        },
                        access: FileSystemAccessMode::Read,
                        missing_path_behavior: None,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: "**/.env".to_string(),
                        },
                        access: FileSystemAccessMode::Deny,
                        missing_path_behavior: None,
                    },
                ],
                glob_scan_max_depth: None,
            },
        };
        let session = ThreadSessionState {
            ..test_thread_session(thread_id, test_path_buf("/tmp/main"))
                .with_native_permission_profile(profile.clone())
        };

        app.primary_thread_id = Some(thread_id);
        app.active_thread_id = Some(thread_id);
        app.primary_session_configured = Some(session.clone());
        app.thread_event_channels.insert(
            thread_id,
            ThreadEventChannel::new_with_session(/*capacity*/ 4, session.clone(), Vec::new()),
        );
        app.chat_widget.handle_thread_session(session.clone());
        app.config.permissions.approval_policy =
            codex_config::Constrained::allow_any(AskForApproval::OnRequest.to_core());

        app.sync_active_thread_permission_settings_to_cached_session()
            .await;

        let expected_session = ThreadSessionState {
            approval_policy: AskForApproval::OnRequest,
            ..session.with_native_permission_profile(profile)
        };
        assert_eq!(
            app.primary_session_configured,
            Some(expected_session.clone())
        );

        let store_session = app
            .thread_event_channels
            .get(&thread_id)
            .expect("thread channel")
            .store
            .lock()
            .await
            .session
            .clone();
        assert_eq!(store_session, Some(expected_session));
    }

    #[tokio::test]
    async fn service_tier_sync_updates_active_cached_session() {
        let mut app = make_test_app().await;
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000406").expect("valid thread");
        let session = ThreadSessionState {
            service_tier: Some(ServiceTier::Fast.request_value().to_string()),
            ..test_thread_session(thread_id, test_path_buf("/tmp/main"))
        };

        app.primary_thread_id = Some(thread_id);
        app.active_thread_id = Some(thread_id);
        app.primary_session_configured = Some(session.clone());
        app.thread_event_channels.insert(
            thread_id,
            ThreadEventChannel::new_with_session(/*capacity*/ 4, session.clone(), Vec::new()),
        );
        app.chat_widget.handle_thread_session(session);
        app.chat_widget.set_service_tier(/*service_tier*/ None);

        app.sync_active_thread_service_tier_to_cached_session()
            .await;

        let expected_session = ThreadSessionState {
            service_tier: None,
            ..test_thread_session(thread_id, test_path_buf("/tmp/main"))
        };
        assert_eq!(
            app.primary_session_configured,
            Some(expected_session.clone())
        );

        let store_session = app
            .thread_event_channels
            .get(&thread_id)
            .expect("thread channel")
            .store
            .lock()
            .await
            .session
            .clone();
        assert_eq!(store_session, Some(expected_session));
    }

    #[tokio::test]
    async fn thread_read_fallback_uses_active_permission_settings() {
        let mut app = make_test_app().await;
        let primary_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000404").expect("valid thread");
        let read_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000405").expect("valid thread");
        let primary_session = ThreadSessionState {
            ..test_thread_session(primary_thread_id, test_path_buf("/tmp/primary"))
                .with_native_permission_profile(PermissionProfile::workspace_write())
        };
        let read_thread = Thread {
            id: read_thread_id.to_string(),
            extra: None,
            session_id: read_thread_id.to_string(),
            forked_from_id: None,
            parent_thread_id: None,
            preview: "read thread".to_string(),
            ephemeral: false,
            section: None,
            section_entered_at: None,
            project_id: None,
            history_mode: Default::default(),
            model_provider: "read-provider".to_string(),
            model: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 2,
            recency_at: Some(2),
            status: codex_app_server_protocol::ThreadStatus::Idle,
            path: None,
            cwd: test_path_buf("/tmp/read").abs().into(),
            cli_version: "0.0.0".to_string(),
            source: codex_app_server_protocol::SessionSource::Unknown,
            can_accept_direct_input: None,
            thread_source: None,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: Some("read thread".to_string()),
            turns: Vec::new(),
        };

        app.primary_session_configured = Some(primary_session.clone());
        app.chat_widget.handle_thread_session(primary_session);

        let session = app
            .session_state_for_thread_read(read_thread_id, &read_thread)
            .await;

        let expected_permission_profile = app
            .chat_widget
            .config_ref()
            .permissions
            .permission_profile()
            .clone();
        assert_eq!(
            session.native_permission_profile(),
            Some(&expected_permission_profile)
        );
        assert_ne!(
            session.native_permission_profile(),
            Some(app.config.permissions.permission_profile()),
            "thread/read fallback must use the active widget permissions rather than stale app \
             config defaults"
        );
    }
}
