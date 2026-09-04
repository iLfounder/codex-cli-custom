use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::SandboxPolicy;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_utils_path_uri::LegacyAppPathString;

use crate::session_state::ThreadSessionState;

/// Server-authoritative values used by status surfaces for a remote thread.
///
/// Paths remain in their execution host's convention and are never converted
/// to a local `PathBuf`.
#[derive(Clone, Debug)]
pub(crate) struct RemoteRuntimeStatus {
    cwd: LegacyAppPathString,
    runtime_workspace_roots: Vec<LegacyAppPathString>,
    sandbox: SandboxPolicy,
    active_permission_profile: Option<ActivePermissionProfile>,
    approval_policy: AskForApproval,
    approvals_reviewer: ApprovalsReviewer,
    model_provider_id: String,
    requires_openai_auth: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct StatusRuntimeDisplay {
    pub(crate) directory: String,
    pub(crate) permissions: String,
    pub(crate) model_provider_id: String,
    pub(crate) requires_openai_auth: bool,
}

impl RemoteRuntimeStatus {
    pub(crate) fn from_session(
        session: &ThreadSessionState,
        requires_openai_auth: bool,
    ) -> Option<Self> {
        Some(Self {
            cwd: session.remote_cwd()?.clone(),
            runtime_workspace_roots: session
                .remote_runtime_workspace_roots()
                .unwrap_or_default()
                .to_vec(),
            sandbox: session.remote_sandbox()?.clone(),
            active_permission_profile: session.active_permission_profile.clone(),
            approval_policy: session.approval_policy,
            approvals_reviewer: session.approvals_reviewer,
            model_provider_id: session.model_provider_id.clone(),
            requires_openai_auth,
        })
    }

    pub(crate) fn update(
        &mut self,
        cwd: LegacyAppPathString,
        sandbox: SandboxPolicy,
        active_permission_profile: Option<ActivePermissionProfile>,
        approval_policy: AskForApproval,
        approvals_reviewer: ApprovalsReviewer,
        model_provider_id: String,
    ) {
        if self.runtime_workspace_roots.as_slice() == [self.cwd.clone()] {
            self.runtime_workspace_roots = vec![cwd.clone()];
        }
        self.cwd = cwd;
        self.sandbox = sandbox;
        self.active_permission_profile = active_permission_profile;
        self.approval_policy = approval_policy;
        self.approvals_reviewer = approvals_reviewer;
        self.model_provider_id = model_provider_id;
    }

    pub(crate) fn active_permission_profile_id(&self) -> Option<String> {
        self.active_permission_profile
            .as_ref()
            .map(|profile| profile.id.clone())
    }

    pub(crate) fn directory_display(&self) -> String {
        self.cwd.render_for_ui()
    }

    pub(crate) fn project_root_name(&self) -> Option<String> {
        let cwd_uri = self.cwd.to_inferred_path_uri()?;
        let root = self
            .runtime_workspace_roots
            .iter()
            .filter_map(LegacyAppPathString::to_inferred_path_uri)
            .filter(|root| cwd_uri.starts_with(root))
            .max_by_key(|root| root.lexical_depth().unwrap_or_default());
        root.as_ref()
            .unwrap_or(&cwd_uri)
            .basename()
            .or_else(|| Some(self.cwd.render_for_ui()))
    }

    pub(crate) fn compact_permissions_label(&self) -> String {
        if let Some(profile) = self.active_permission_profile.as_ref()
            && !profile.id.starts_with(':')
        {
            return profile.id.clone();
        }

        match &self.sandbox {
            SandboxPolicy::ReadOnly {
                network_access: false,
            } => "Read Only".to_string(),
            SandboxPolicy::WorkspaceWrite {
                network_access: false,
                ..
            } => "Workspace".to_string(),
            SandboxPolicy::DangerFullAccess => "Full Access".to_string(),
            SandboxPolicy::ReadOnly { .. }
            | SandboxPolicy::WorkspaceWrite { .. }
            | SandboxPolicy::ExternalSandbox { .. } => "Custom permissions".to_string(),
        }
    }

    pub(crate) fn approval_mode_label(&self) -> String {
        if self.approval_policy == AskForApproval::OnRequest {
            return match self.approvals_reviewer {
                ApprovalsReviewer::AutoReview => "Approve for me".to_string(),
                ApprovalsReviewer::User => "Ask for approval".to_string(),
            };
        }

        self.approval_policy.to_core().to_string()
    }

    pub(crate) fn status_display(&self) -> StatusRuntimeDisplay {
        StatusRuntimeDisplay {
            directory: self.directory_display(),
            permissions: self.status_permissions_label(),
            model_provider_id: self.model_provider_id.clone(),
            requires_openai_auth: self.requires_openai_auth,
        }
    }

    fn status_permissions_label(&self) -> String {
        let approval = self.approval_mode_label();
        let sandbox = self.sandbox_label();
        let workspace_root_suffix = self.workspace_root_suffix();
        let active_id = self
            .active_permission_profile
            .as_ref()
            .map(|profile| profile.id.as_str());

        match active_id {
            Some(BUILT_IN_PERMISSION_PROFILE_READ_ONLY) => {
                let label = if sandbox == "read-only with network access" {
                    "Read Only with network access"
                } else {
                    "Read Only"
                };
                format!("{label} ({approval})")
            }
            Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE) => {
                let label = if sandbox == "workspace with network access" {
                    "Workspace with network access"
                } else {
                    "Workspace"
                };
                format!(
                    "{label}{} ({approval})",
                    workspace_root_suffix.as_deref().unwrap_or("")
                )
            }
            Some(BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS) => {
                if self.approval_policy == AskForApproval::Never {
                    "Full Access".to_string()
                } else {
                    format!("No Sandbox ({approval})")
                }
            }
            Some(id) => format!(
                "Profile {id} ({}{}, {approval})",
                sandbox,
                if sandbox.starts_with("workspace") {
                    workspace_root_suffix.as_deref().unwrap_or("")
                } else {
                    ""
                }
            ),
            None => match &self.sandbox {
                SandboxPolicy::ReadOnly { network_access } => {
                    let label = if *network_access {
                        "Read Only with network access"
                    } else {
                        "Read Only"
                    };
                    format!("{label} ({approval})")
                }
                SandboxPolicy::WorkspaceWrite { network_access, .. } => {
                    let label = if *network_access {
                        "Workspace with network access"
                    } else {
                        "Workspace"
                    };
                    format!(
                        "{label}{} ({approval})",
                        workspace_root_suffix.as_deref().unwrap_or("")
                    )
                }
                SandboxPolicy::DangerFullAccess => {
                    if self.approval_policy == AskForApproval::Never {
                        "Full Access".to_string()
                    } else {
                        format!("No Sandbox ({approval})")
                    }
                }
                SandboxPolicy::ExternalSandbox { .. } => {
                    format!("Custom ({sandbox}, {approval})")
                }
            },
        }
    }

    fn sandbox_label(&self) -> &'static str {
        match &self.sandbox {
            SandboxPolicy::DangerFullAccess => "danger-full-access",
            SandboxPolicy::ReadOnly {
                network_access: false,
            } => "read-only",
            SandboxPolicy::ReadOnly {
                network_access: true,
            } => "read-only with network access",
            SandboxPolicy::WorkspaceWrite {
                network_access: false,
                ..
            } => "workspace",
            SandboxPolicy::WorkspaceWrite {
                network_access: true,
                ..
            } => "workspace with network access",
            SandboxPolicy::ExternalSandbox { .. } => "external sandbox",
        }
    }

    fn workspace_root_suffix(&self) -> Option<String> {
        let extra_roots = self
            .runtime_workspace_roots
            .iter()
            .filter(|root| *root != &self.cwd)
            .map(LegacyAppPathString::render_for_ui)
            .collect::<Vec<_>>();
        (!extra_roots.is_empty()).then(|| format!(" [{}]", extra_roots.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::NetworkAccess;

    #[test]
    fn remote_status_preserves_foreign_paths_and_runtime_authority() {
        let status = RemoteRuntimeStatus {
            cwd: LegacyAppPathString::from_string("/Users/daniel/work/repo/subdir"),
            runtime_workspace_roots: vec![
                LegacyAppPathString::from_string("/Users/daniel/work/repo"),
                LegacyAppPathString::from_string("/Volumes/shared"),
            ],
            sandbox: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            active_permission_profile: Some(ActivePermissionProfile {
                id: BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string(),
                extends: None,
            }),
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: ApprovalsReviewer::AutoReview,
            model_provider_id: "remote-provider".to_string(),
            requires_openai_auth: false,
        };

        assert_eq!(status.directory_display(), "/Users/daniel/work/repo/subdir");
        assert_eq!(status.project_root_name().as_deref(), Some("repo"));
        assert_eq!(status.compact_permissions_label(), "Workspace");
        assert_eq!(status.approval_mode_label(), "Approve for me");
        let display = status.status_display();
        assert!(display.permissions.contains("/Volumes/shared"));
        assert_eq!(display.model_provider_id, "remote-provider");
        assert!(!display.requires_openai_auth);

        let external = RemoteRuntimeStatus {
            sandbox: SandboxPolicy::ExternalSandbox {
                network_access: NetworkAccess::Enabled,
            },
            ..status
        };
        assert_eq!(external.compact_permissions_label(), "Custom permissions");
    }
}
