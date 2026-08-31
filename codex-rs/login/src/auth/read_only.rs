use super::agent_identity::agent_identity_authapi_base_url;
use super::manager::AuthConfig;
use super::manager::CodexAuth;
use super::manager::ensure_auth_workspace_allowed;
use super::storage::CredentialRevision;
use super::storage::read_auth_file_snapshot;
use codex_config::types::AuthCredentialsStoreMode;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;

pub(super) struct LoadedAuth {
    pub(super) auth: Option<CodexAuth>,
    pub(super) credential_revision: Option<CredentialRevision>,
}

pub(super) async fn load_auth(auth_config: &AuthConfig) -> std::io::Result<LoadedAuth> {
    let Some(snapshot) = read_auth_file_snapshot(&auth_config.codex_home)? else {
        return Ok(LoadedAuth {
            auth: None,
            credential_revision: None,
        });
    };
    if snapshot.auth.resolved_mode() == AuthMode::AgentIdentity {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "read-only sibling auth cannot initialize agent identity",
        ));
    }
    let login_method = if snapshot.auth.resolved_mode().uses_codex_backend() {
        ForcedLoginMethod::Chatgpt
    } else {
        ForcedLoginMethod::Api
    };
    if !auth_config.is_login_method_allowed(login_method) {
        return Ok(LoadedAuth {
            auth: None,
            credential_revision: Some(snapshot.revision),
        });
    }
    let effective_workspaces = auth_config.effective_chatgpt_workspaces();
    let agent_identity_authapi_base_url =
        agent_identity_authapi_base_url(auth_config.chatgpt_base_url.as_deref()).ok();
    let auth = CodexAuth::from_auth_dot_json(
        &auth_config.codex_home,
        snapshot.auth,
        AuthCredentialsStoreMode::File,
        auth_config.chatgpt_base_url.as_deref(),
        auth_config.keyring_backend_kind,
        agent_identity_authapi_base_url.as_deref(),
        &auth_config.auth_route_config,
    )
    .await?;
    if let CodexAuth::PersonalAccessToken(auth) = &auth {
        ensure_auth_workspace_allowed(effective_workspaces.as_deref(), auth.account_id())?;
    }
    Ok(LoadedAuth {
        auth: auth_config.allows_auth(&auth).then_some(auth),
        credential_revision: Some(snapshot.revision),
    })
}

#[cfg(test)]
#[path = "read_only_tests.rs"]
mod tests;
