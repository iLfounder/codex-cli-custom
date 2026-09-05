//! Thread settings sync between TUI-local state and app-server thread state.

use super::App;
use crate::app_command::AppCommand;
use crate::app_event::AppEvent;
use crate::app_server_session::AppServerSession;
use crate::chatwidget::cyber_model_approval_reviewer;
use crate::session_state::ThreadSessionState;
use codex_app_server_protocol::ApprovalsReviewer as AppServerApprovalsReviewer;
use codex_app_server_protocol::AskForApproval as AppServerAskForApproval;
use codex_app_server_protocol::ThreadSettings;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_config::types::ApprovalsReviewer;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::MODEL_SPECIALTY_CYBER;
use codex_utils_path_uri::LegacyAppPathString;

impl App {
    pub(super) async fn sync_active_thread_model_setting(
        &mut self,
        app_server: &mut AppServerSession,
        model: String,
        effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    ) {
        let Some(mut params) = self.active_thread_model_setting_update_params(model) else {
            return;
        };
        if !self.app_server_target.uses_remote_workspace() {
            params.effort = effort;
        }
        let defaulted_to_auto_review = params.approvals_reviewer
            == Some(AppServerApprovalsReviewer::AutoReview)
            && (self.chat_widget.config_ref().approvals_reviewer != ApprovalsReviewer::AutoReview
                || AppServerAskForApproval::from(
                    self.chat_widget
                        .config_ref()
                        .permissions
                        .approval_policy
                        .value(),
                ) != AppServerAskForApproval::OnRequest);
        let settings_updated = self.send_thread_settings_update(app_server, params).await;
        if defaulted_to_auto_review && settings_updated {
            self.app_event_tx.send(AppEvent::CyberModelAutoReviewNotice);
        }
    }

    pub(super) fn active_thread_model_setting_update_params(
        &self,
        model: String,
    ) -> Option<ThreadSettingsUpdateParams> {
        let thread_id = self.active_thread_id?;
        if self.app_server_target.uses_remote_workspace() {
            return Some(ThreadSettingsUpdateParams {
                thread_id: thread_id.to_string(),
                model: Some(model),
                ..ThreadSettingsUpdateParams::default()
            });
        }
        let is_cyber_model = self.model_catalog.try_list_models().is_ok_and(|models| {
            models.iter().any(|preset| {
                preset.model == model
                    && preset.model_specialty.as_deref() == Some(MODEL_SPECIALTY_CYBER)
            })
        });

        let mut params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            model: Some(model),
            collaboration_mode: Some(self.chat_widget.effective_collaboration_mode()),
            ..ThreadSettingsUpdateParams::default()
        };

        if is_cyber_model {
            let workspace_profile = PermissionProfile::workspace_write();
            let workspace_allowed = self
                .config
                .permissions
                .can_set_permission_profile(&workspace_profile)
                .is_ok()
                && self.config.is_permission_profile_allowed(
                    BUILT_IN_PERMISSION_PROFILE_WORKSPACE,
                    &workspace_profile,
                );

            if workspace_allowed && let Some(reviewer) = cyber_model_approval_reviewer(&self.config)
            {
                params.permissions = Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string());
                params.approval_policy = Some(AppServerAskForApproval::OnRequest);
                params.approvals_reviewer = Some(reviewer.into());
            }
        }

        Some(params)
    }

    pub(super) async fn sync_active_thread_reasoning_setting(
        &mut self,
        app_server: &mut AppServerSession,
        effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    ) {
        let Some(params) = self.active_thread_reasoning_setting_update_params(effort) else {
            return;
        };
        self.send_thread_settings_update(app_server, params).await;
    }

    pub(super) fn active_thread_reasoning_setting_update_params(
        &self,
        effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    ) -> Option<ThreadSettingsUpdateParams> {
        let thread_id = self.active_thread_id?;
        if self.app_server_target.uses_remote_workspace() {
            return Some(ThreadSettingsUpdateParams {
                thread_id: thread_id.to_string(),
                effort,
                ..ThreadSettingsUpdateParams::default()
            });
        }
        Some(ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            effort,
            collaboration_mode: Some(self.chat_widget.current_collaboration_mode().clone()),
            ..ThreadSettingsUpdateParams::default()
        })
    }

    pub(super) async fn sync_active_thread_plan_mode_reasoning_setting(
        &mut self,
        app_server: &mut AppServerSession,
    ) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            collaboration_mode: Some(self.chat_widget.effective_collaboration_mode()),
            ..ThreadSettingsUpdateParams::default()
        };
        self.send_thread_settings_update(app_server, params).await;
    }

    pub(super) async fn sync_active_thread_personality_setting(
        &mut self,
        app_server: &mut AppServerSession,
        personality: codex_protocol::config_types::Personality,
    ) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            personality: Some(personality),
            ..ThreadSettingsUpdateParams::default()
        };
        self.send_thread_settings_update(app_server, params).await;
    }

    pub(super) async fn sync_override_turn_context_settings(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        op: &AppCommand,
    ) {
        let AppCommand::OverrideTurnContext {
            cwd,
            approval_policy,
            approvals_reviewer,
            permission_profile: _,
            active_permission_profile,
            // TODO(anp): Support Windows sandbox updates through environment configuration;
            // thread/settings/update cannot currently represent this override.
            windows_sandbox_level: _,
            model,
            effort,
            summary,
            service_tier,
            collaboration_mode,
            personality,
        } = op
        else {
            return;
        };

        let params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            cwd: cwd
                .as_ref()
                .map(|cwd| LegacyAppPathString::from_path(cwd.as_path())),
            approval_policy: *approval_policy,
            approvals_reviewer: approvals_reviewer.map(AppServerApprovalsReviewer::from),
            permissions: active_permission_profile
                .as_ref()
                .map(|profile| profile.id.clone()),
            model: model.clone(),
            effort: effort.clone().unwrap_or_default(),
            summary: *summary,
            service_tier: service_tier.clone(),
            collaboration_mode: collaboration_mode.clone(),
            personality: *personality,
            ..ThreadSettingsUpdateParams::default()
        };
        self.send_thread_settings_update(app_server, params).await;
    }

    pub(super) async fn apply_thread_settings_to_cached_session(
        &mut self,
        thread_id: ThreadId,
        settings: &ThreadSettings,
    ) -> Result<(), String> {
        if self.primary_thread_id == Some(thread_id)
            && let Some(session) = self.primary_session_configured.as_mut()
        {
            apply_thread_settings_to_session(session, settings)?;
        }

        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            let mut store = channel.store.lock().await;
            if let Some(session) = store.session.as_mut() {
                apply_thread_settings_to_session(session, settings)?;
            }
        }
        Ok(())
    }

    pub(super) async fn send_thread_settings_update(
        &mut self,
        app_server: &mut AppServerSession,
        params: ThreadSettingsUpdateParams,
    ) -> bool {
        if !thread_settings_update_has_changes(&params) {
            return false;
        }
        match app_server.thread_settings_update(params).await {
            Ok(settings_updated) => settings_updated,
            Err(err) => {
                tracing::warn!("failed to update app-server thread settings from TUI: {err}");
                self.chat_widget
                    .add_error_message(format!("Failed to update thread settings: {err}"));
                false
            }
        }
    }
}

fn apply_thread_settings_to_session(
    session: &mut ThreadSessionState,
    settings: &ThreadSettings,
) -> Result<(), String> {
    let native_sandbox = session
        .native_cwd()
        .is_some()
        .then(|| settings.sandbox_policy.try_to_core())
        .transpose()
        .map_err(|error| format!("server returned a non-native sandbox path: {error}"))?;
    if settings.collaboration_mode.mode == ModeKind::Default {
        session.model = settings.model.clone();
        session.reasoning_effort = settings.effort.clone();
    }
    session.model_provider_id = settings.model_provider.clone();
    session.service_tier = settings.service_tier.clone();
    session.approval_policy = settings.approval_policy;
    session.approvals_reviewer = settings.approvals_reviewer.to_core();
    if let Some(cwd) = session.native_cwd().cloned() {
        session.set_native_permission_profile(
            PermissionProfile::from_legacy_sandbox_policy_for_cwd(
                native_sandbox
                    .as_ref()
                    .expect("native sessions validate sandbox paths"),
                cwd.as_path(),
            ),
        );
        if let Some(updated_cwd) = settings.cwd.to_inferred_abs_path() {
            session.set_cwd_retargeting_implicit_runtime_workspace_root(updated_cwd);
        }
    } else {
        let roots = session
            .remote_runtime_workspace_roots()
            .unwrap_or_default()
            .to_vec();
        session.replace_remote_execution_context(
            settings.cwd.clone(),
            roots,
            settings.sandbox_policy.clone(),
        );
    }
    session.active_permission_profile = settings.active_permission_profile.clone().map(Into::into);
    session.personality = settings.personality;
    let mut collaboration_mode = settings.collaboration_mode.clone();
    collaboration_mode
        .settings
        .model
        .clone_from(&settings.model);
    collaboration_mode.settings.reasoning_effort = settings.effort.clone();
    session.collaboration_mode = Some(Box::new(collaboration_mode));
    Ok(())
}

fn thread_settings_update_has_changes(params: &ThreadSettingsUpdateParams) -> bool {
    params.cwd.is_some()
        || params.approval_policy.is_some()
        || params.approvals_reviewer.is_some()
        || params.sandbox_policy.is_some()
        || params.permissions.is_some()
        || params.model.is_some()
        || params.service_tier.is_some()
        || params.effort.is_some()
        || params.summary.is_some()
        || params.collaboration_mode.is_some()
        || params.personality.is_some()
}
