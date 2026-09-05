//! Canonical TUI session state shared across app-server routing, chat display, and status UI.
//!
//! The app-server API is the boundary for session lifecycle events. Once those responses enter
//! TUI, this module holds the small internal state shape used by app orchestration and widgets.

use std::path::PathBuf;

use codex_app_server_protocol::AskForApproval;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SessionExecutionContext {
    Native {
        cwd: AbsolutePathBuf,
        runtime_workspace_roots: Vec<AbsolutePathBuf>,
        permission_profile: PermissionProfile,
        rollout_path: Option<PathBuf>,
    },
    Remote {
        cwd: LegacyAppPathString,
        runtime_workspace_roots: Vec<LegacyAppPathString>,
        sandbox: codex_app_server_protocol::SandboxPolicy,
        rollout_path: Option<LegacyAppPathString>,
    },
}

impl SessionExecutionContext {
    pub(crate) fn native(
        cwd: AbsolutePathBuf,
        runtime_workspace_roots: Vec<AbsolutePathBuf>,
        permission_profile: PermissionProfile,
        rollout_path: Option<PathBuf>,
    ) -> Self {
        Self::Native {
            cwd,
            runtime_workspace_roots,
            permission_profile,
            rollout_path,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionNetworkProxyRuntime {
    pub(crate) http_addr: String,
    pub(crate) socks_addr: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MessageHistoryMetadata {
    pub(crate) log_id: u64,
    pub(crate) entry_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThreadSessionState {
    pub(crate) thread_id: ThreadId,
    pub(crate) forked_from_id: Option<ThreadId>,
    pub(crate) fork_parent_title: Option<String>,
    pub(crate) thread_name: Option<String>,
    pub(crate) model: String,
    pub(crate) model_provider_id: String,
    pub(crate) service_tier: Option<String>,
    pub(crate) approval_policy: AskForApproval,
    pub(crate) approvals_reviewer: codex_protocol::config_types::ApprovalsReviewer,
    /// Named or implicit built-in permission profile, when the server knows it.
    pub(crate) active_permission_profile: Option<ActivePermissionProfile>,
    pub(crate) execution_context: SessionExecutionContext,
    pub(crate) instruction_source_paths: Vec<PathUri>,
    pub(crate) reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    pub(crate) collaboration_mode: Option<Box<CollaborationMode>>,
    pub(crate) personality: Option<Personality>,
    pub(crate) message_history: Option<MessageHistoryMetadata>,
    pub(crate) network_proxy: Option<SessionNetworkProxyRuntime>,
}

impl ThreadSessionState {
    pub(crate) fn native_cwd(&self) -> Option<&AbsolutePathBuf> {
        match &self.execution_context {
            SessionExecutionContext::Native { cwd, .. } => Some(cwd),
            SessionExecutionContext::Remote { .. } => None,
        }
    }

    pub(crate) fn remote_cwd(&self) -> Option<&LegacyAppPathString> {
        match &self.execution_context {
            SessionExecutionContext::Remote { cwd, .. } => Some(cwd),
            SessionExecutionContext::Native { .. } => None,
        }
    }

    pub(crate) fn display_cwd(&self) -> String {
        match &self.execution_context {
            SessionExecutionContext::Native { cwd, .. } => cwd.display().to_string(),
            SessionExecutionContext::Remote { cwd, .. } => cwd.render_for_ui(),
        }
    }

    pub(crate) fn server_cwd(&self) -> LegacyAppPathString {
        match &self.execution_context {
            SessionExecutionContext::Native { cwd, .. } => LegacyAppPathString::from_abs_path(cwd),
            SessionExecutionContext::Remote { cwd, .. } => cwd.clone(),
        }
    }

    pub(crate) fn native_permission_profile(&self) -> Option<&PermissionProfile> {
        match &self.execution_context {
            SessionExecutionContext::Native {
                permission_profile, ..
            } => Some(permission_profile),
            SessionExecutionContext::Remote { .. } => None,
        }
    }

    pub(crate) fn set_native_permission_profile(&mut self, profile: PermissionProfile) {
        if let SessionExecutionContext::Native {
            permission_profile, ..
        } = &mut self.execution_context
        {
            *permission_profile = profile;
        }
    }

    pub(crate) fn with_native_permission_profile(mut self, profile: PermissionProfile) -> Self {
        self.set_native_permission_profile(profile);
        self
    }

    pub(crate) fn set_native_rollout_path(&mut self, path: Option<PathBuf>) {
        if let SessionExecutionContext::Native { rollout_path, .. } = &mut self.execution_context {
            *rollout_path = path;
        }
    }

    pub(crate) fn with_native_rollout_path(mut self, path: Option<PathBuf>) -> Self {
        self.set_native_rollout_path(path);
        self
    }

    pub(crate) fn set_native_cwd_and_roots(
        &mut self,
        cwd: AbsolutePathBuf,
        runtime_workspace_roots: Vec<AbsolutePathBuf>,
    ) {
        if let SessionExecutionContext::Native {
            cwd: current_cwd,
            runtime_workspace_roots: current_roots,
            ..
        } = &mut self.execution_context
        {
            *current_cwd = cwd;
            *current_roots = runtime_workspace_roots;
        }
    }

    pub(crate) fn with_native_cwd_and_roots(
        mut self,
        cwd: AbsolutePathBuf,
        runtime_workspace_roots: Vec<AbsolutePathBuf>,
    ) -> Self {
        self.set_native_cwd_and_roots(cwd, runtime_workspace_roots);
        self
    }

    pub(crate) fn remote_sandbox(&self) -> Option<&codex_app_server_protocol::SandboxPolicy> {
        match &self.execution_context {
            SessionExecutionContext::Remote { sandbox, .. } => Some(sandbox),
            SessionExecutionContext::Native { .. } => None,
        }
    }

    pub(crate) fn native_rollout_path(&self) -> Option<&std::path::Path> {
        match &self.execution_context {
            SessionExecutionContext::Native { rollout_path, .. } => rollout_path.as_deref(),
            SessionExecutionContext::Remote { .. } => None,
        }
    }

    pub(crate) fn remote_rollout_path(&self) -> Option<&LegacyAppPathString> {
        match &self.execution_context {
            SessionExecutionContext::Remote { rollout_path, .. } => rollout_path.as_ref(),
            SessionExecutionContext::Native { .. } => None,
        }
    }

    pub(crate) fn native_runtime_workspace_roots(&self) -> Option<&[AbsolutePathBuf]> {
        match &self.execution_context {
            SessionExecutionContext::Native {
                runtime_workspace_roots,
                ..
            } => Some(runtime_workspace_roots),
            SessionExecutionContext::Remote { .. } => None,
        }
    }

    pub(crate) fn remote_runtime_workspace_roots(&self) -> Option<&[LegacyAppPathString]> {
        match &self.execution_context {
            SessionExecutionContext::Remote {
                runtime_workspace_roots,
                ..
            } => Some(runtime_workspace_roots),
            SessionExecutionContext::Native { .. } => None,
        }
    }

    pub(crate) fn set_cwd_retargeting_implicit_runtime_workspace_root(
        &mut self,
        cwd: AbsolutePathBuf,
    ) {
        let SessionExecutionContext::Native {
            cwd: current_cwd,
            runtime_workspace_roots,
            ..
        } = &mut self.execution_context
        else {
            return;
        };
        let previous_cwd = std::mem::replace(current_cwd, cwd.clone());
        if !runtime_workspace_roots.contains(&previous_cwd) {
            return;
        }

        let previous_roots = std::mem::take(runtime_workspace_roots);
        runtime_workspace_roots.push(cwd);
        for root in previous_roots {
            if root != previous_cwd && !runtime_workspace_roots.contains(&root) {
                runtime_workspace_roots.push(root);
            }
        }
    }

    pub(crate) fn replace_remote_execution_context(
        &mut self,
        cwd: LegacyAppPathString,
        runtime_workspace_roots: Vec<LegacyAppPathString>,
        sandbox: codex_app_server_protocol::SandboxPolicy,
    ) {
        let SessionExecutionContext::Remote {
            cwd: current_cwd,
            runtime_workspace_roots: current_roots,
            sandbox: current_sandbox,
            ..
        } = &mut self.execution_context
        else {
            return;
        };
        *current_cwd = cwd;
        *current_roots = runtime_workspace_roots;
        *current_sandbox = sandbox;
    }

    pub(crate) fn set_remote_cwd_and_rollout(
        &mut self,
        cwd: LegacyAppPathString,
        rollout: Option<LegacyAppPathString>,
    ) {
        if let SessionExecutionContext::Remote {
            cwd: current_cwd,
            rollout_path,
            ..
        } = &mut self.execution_context
        {
            *current_cwd = cwd;
            *rollout_path = rollout;
        }
    }
}
