use crate::auth::manager::AgentIdentityAuthPolicy;
use crate::auth::manager::AuthConfig;
use crate::auth::manager::AuthManager;
use crate::auth::manager::CodexAuth;
use crate::auth::manager::ExternalAuth;
use crate::auth::manager::ExternalAuthFuture;
use crate::auth::manager::ExternalAuthRefreshContext;
use crate::auth::manager::ReadOnlyAuthRefresh;
use crate::auth::storage::AuthKeyringBackendKind;
use crate::auth::storage::AuthStorageBackend;
use crate::auth::storage::FileAuthStorage;
use crate::auth::storage::get_auth_file;
use base64::Engine;
use chrono::Utc;
use codex_config::ManagedAuthPolicy;
use codex_config::types::AuthCredentialsStoreMode;
use codex_protocol::protocol::SessionSource;
use serde::Serialize;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

fn read_only_test_auth_config(codex_home: &Path) -> AuthConfig {
    AuthConfig {
        codex_home: codex_home.to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: AuthKeyringBackendKind::default(),
        forced_login_method: None,
        chatgpt_base_url: None,
        forced_chatgpt_workspace_id: None,
        managed_auth_policy: ManagedAuthPolicy::default(),
        auth_route_config: crate::test_support::transport_default_auth_route_config(),
    }
}

fn write_private_chatgpt_auth(
    codex_home: &Path,
    account_id: &str,
    access_token: &str,
) -> std::io::Result<()> {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header = encode(&serde_json::to_vec(&Header {
        alg: "none",
        typ: "JWT",
    })?);
    let payload = encode(&serde_json::to_vec(&json!({
        "email": "user@example.com",
        "email_verified": true,
        "https://api.openai.com/auth": {
            "chatgpt_user_id": "user-12345",
            "user_id": "user-12345",
            "chatgpt_plan_type": "pro",
            "chatgpt_account_id": account_id,
        },
    }))?);
    let id_token = format!("{header}.{payload}.{}", encode(b"sig"));
    std::fs::write(
        get_auth_file(codex_home),
        serde_json::to_vec_pretty(&json!({
            "tokens": {
                "id_token": id_token,
                "access_token": access_token,
                "refresh_token": "test-refresh-token",
                "account_id": account_id,
            },
            "last_refresh": Utc::now(),
        }))?,
    )?;
    let storage = FileAuthStorage::new(codex_home.to_path_buf());
    let auth = storage
        .load()?
        .ok_or_else(|| std::io::Error::other("test auth was not written"))?;
    storage.save(&auth)
}

#[derive(Clone)]
struct StaticExternalAuth(CodexAuth);

impl ExternalAuth for StaticExternalAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

struct ReplacingReadOnlyRefresh {
    auth_home: PathBuf,
    account_id: &'static str,
    access_token: &'static str,
    calls: AtomicUsize,
}

struct NoopReadOnlyRefresh(AtomicUsize);

impl ReadOnlyAuthRefresh for NoopReadOnlyRefresh {
    fn force_refresh(&self) -> ExternalAuthFuture<'_, ()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

impl ReadOnlyAuthRefresh for ReplacingReadOnlyRefresh {
    fn force_refresh(&self) -> ExternalAuthFuture<'_, ()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result =
            write_private_chatgpt_auth(&self.auth_home, self.account_id, self.access_token);
        Box::pin(async move { result })
    }
}

#[tokio::test]
async fn sibling_auth_rejects_every_write_path() {
    let auth_home = tempdir().expect("temp auth home");
    write_private_chatgpt_auth(auth_home.path(), "account-a", "access-a")
        .expect("write private auth");
    let auth_file = get_auth_file(auth_home.path());
    let original = std::fs::read(&auth_file).expect("read private auth");
    let manager = AuthManager::shared_from_read_only_auth_config(read_only_test_auth_config(
        auth_home.path(),
    ))
    .await
    .expect("create read-only manager");

    let revision = manager.credential_revision().expect("credential revision");
    assert_eq!(format!("{revision:?}"), "CredentialRevision(REDACTED)");
    assert_eq!(
        manager
            .auth()
            .await
            .and_then(|auth| auth.get_token().ok())
            .as_deref(),
        Some("access-a")
    );
    assert!(manager.refresh_token().await.is_err());
    assert!(manager.logout().await.is_err());
    assert!(
        manager
            .agent_identity_auth(AgentIdentityAuthPolicy::ChatGptAuth, SessionSource::Cli,)
            .await
            .is_err()
    );
    assert!(
        manager
            .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(
                "external",
            ))))
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(auth_file).expect("read auth after calls"),
        original
    );
}

#[tokio::test]
async fn unauthorized_recovery_accepts_same_account_file_replacement() {
    let auth_home = tempdir().expect("temp auth home");
    write_private_chatgpt_auth(auth_home.path(), "account-a", "access-a")
        .expect("write private auth");
    let manager = AuthManager::shared_from_read_only_auth_config(read_only_test_auth_config(
        auth_home.path(),
    ))
    .await
    .expect("create read-only manager");
    let initial_revision = manager.credential_revision().expect("initial revision");
    write_private_chatgpt_auth(auth_home.path(), "account-a", "access-b")
        .expect("replace private auth");

    let mut recovery = manager.unauthorized_recovery();
    let result = recovery.next().await.expect("reload replacement auth");

    assert_eq!(result.auth_state_changed(), Some(true));
    assert!(!recovery.has_next());
    assert_ne!(manager.credential_revision(), Some(initial_revision));
    assert_eq!(
        manager
            .auth_cached()
            .and_then(|auth| auth.get_token().ok())
            .as_deref(),
        Some("access-b")
    );
}

#[tokio::test]
async fn unauthorized_recovery_delegates_unchanged_snapshot_and_reloads_replacement() {
    let auth_home = tempdir().expect("temp auth home");
    write_private_chatgpt_auth(auth_home.path(), "account-a", "access-a")
        .expect("write private auth");
    let refresh = Arc::new(ReplacingReadOnlyRefresh {
        auth_home: auth_home.path().to_path_buf(),
        account_id: "account-a",
        access_token: "access-b",
        calls: AtomicUsize::new(0),
    });
    let manager = AuthManager::shared_from_read_only_auth_config_with_refresh(
        read_only_test_auth_config(auth_home.path()),
        refresh.clone(),
    )
    .await
    .expect("create read-only manager");
    let initial_revision = manager.credential_revision();

    let mut recovery = manager.unauthorized_recovery();
    let result = recovery.next().await.expect("refresh replacement auth");

    assert_eq!(result.auth_state_changed(), Some(true));
    assert!(!recovery.has_next());
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 1);
    assert_ne!(manager.credential_revision(), initial_revision);
    assert_eq!(
        manager
            .auth_cached()
            .and_then(|auth| auth.get_token().ok())
            .as_deref(),
        Some("access-b")
    );
}

#[tokio::test]
async fn unauthorized_recovery_rejects_unchanged_snapshot_after_authority_returns() {
    let auth_home = tempdir().expect("temp auth home");
    write_private_chatgpt_auth(auth_home.path(), "account-a", "access-a")
        .expect("write private auth");
    let refresh = Arc::new(NoopReadOnlyRefresh(AtomicUsize::new(0)));
    let manager = AuthManager::shared_from_read_only_auth_config_with_refresh(
        read_only_test_auth_config(auth_home.path()),
        refresh.clone(),
    )
    .await
    .expect("create read-only manager");

    let mut recovery = manager.unauthorized_recovery();
    assert!(recovery.next().await.is_err());

    assert!(!recovery.has_next());
    assert_eq!(refresh.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unauthorized_recovery_rejects_identity_change_written_by_authority() {
    let auth_home = tempdir().expect("temp auth home");
    write_private_chatgpt_auth(auth_home.path(), "account-a", "access-a")
        .expect("write private auth");
    let refresh = Arc::new(ReplacingReadOnlyRefresh {
        auth_home: auth_home.path().to_path_buf(),
        account_id: "account-b",
        access_token: "access-b",
        calls: AtomicUsize::new(0),
    });
    let manager = AuthManager::shared_from_read_only_auth_config_with_refresh(
        read_only_test_auth_config(auth_home.path()),
        refresh.clone(),
    )
    .await
    .expect("create read-only manager");

    let mut recovery = manager.unauthorized_recovery();
    assert!(recovery.next().await.is_err());

    assert!(!recovery.has_next());
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        manager.auth_cached().and_then(|auth| auth.get_account_id()),
        Some("account-a".to_string())
    );
}

#[tokio::test]
async fn unauthorized_recovery_rejects_unchanged_and_mismatched_files() {
    let auth_home = tempdir().expect("temp auth home");
    write_private_chatgpt_auth(auth_home.path(), "account-a", "access-a")
        .expect("write private auth");
    let manager = AuthManager::shared_from_read_only_auth_config(read_only_test_auth_config(
        auth_home.path(),
    ))
    .await
    .expect("create read-only manager");
    let initial_revision = manager.credential_revision();

    let mut unchanged = manager.unauthorized_recovery();
    assert!(unchanged.next().await.is_err());
    assert!(!unchanged.has_next());

    write_private_chatgpt_auth(auth_home.path(), "account-b", "access-b")
        .expect("replace with mismatched account");
    let mut mismatched = manager.unauthorized_recovery();
    assert!(mismatched.next().await.is_err());
    assert!(!mismatched.has_next());
    assert_eq!(manager.credential_revision(), initial_revision);
    assert_eq!(
        manager.auth_cached().and_then(|auth| auth.get_account_id()),
        Some("account-a".to_string())
    );
}
