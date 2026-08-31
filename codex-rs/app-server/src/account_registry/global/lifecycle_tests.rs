use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use base64::Engine;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotListParams;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_config::ManagedAuthPolicy;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::ConfigBuilder;
use codex_login::AuthConfig;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::TokenData;
use codex_login::auth::managed_auth_state;
use codex_login::save_auth;
use codex_login::token_data::IdTokenInfo;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;
use serde_json::Value;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::LIFECYCLE_UNAVAILABLE;
use super::LifecycleAuthority;
use super::LifecycleSession;
use super::exact_chatgpt_account;
use crate::account_registry::AccountRegistry;
use crate::account_registry::global::AccountId;
use crate::account_registry::global::CatalogError;
use crate::account_registry::global::RawSnapshot;
use crate::account_registry::global::TokenManagerClient;
use crate::account_registry::global::directory::subscription_source_ref as canonical_subscription_source_ref;
use crate::config_manager::ConfigManager;

#[derive(Default)]
struct AuthorityState {
    active: BTreeSet<AccountId>,
    actions: Vec<Value>,
    fail_commit: bool,
}

#[derive(Clone, Default)]
struct FixtureAuthority {
    state: Arc<Mutex<AuthorityState>>,
}

impl FixtureAuthority {
    fn failing_commit() -> Self {
        Self {
            state: Arc::new(Mutex::new(AuthorityState {
                fail_commit: true,
                ..AuthorityState::default()
            })),
        }
    }

    fn actions(&self) -> Vec<Value> {
        self.state.lock().expect("authority state").actions.clone()
    }
}

struct FixtureSession {
    authority: FixtureAuthority,
    account_id: AccountId,
}

impl LifecycleAuthority for FixtureAuthority {
    type Session = FixtureSession;

    async fn begin(&self, account_id: AccountId) -> Result<Self::Session, CatalogError> {
        let mut state = self.state.lock().expect("authority state");
        if !state.active.insert(account_id) {
            return Err(CatalogError::Request);
        }
        state.actions.push(serde_json::json!({
            "method": "lifecycle/begin",
            "accountId": account_id.to_string(),
        }));
        Ok(FixtureSession {
            authority: self.clone(),
            account_id,
        })
    }
}

impl LifecycleSession for FixtureSession {
    async fn commit(self) -> Result<(), CatalogError> {
        let mut state = self.authority.state.lock().expect("authority state");
        assert!(state.active.remove(&self.account_id));
        state.actions.push(serde_json::json!({
            "method": "lifecycle/commit",
            "accountId": self.account_id.to_string(),
        }));
        if state.fail_commit {
            Err(CatalogError::Request)
        } else {
            Ok(())
        }
    }

    async fn abort(self) {
        let mut state = self.authority.state.lock().expect("authority state");
        assert!(state.active.remove(&self.account_id));
        state.actions.push(serde_json::json!({
            "method": "lifecycle/abort",
            "accountId": self.account_id.to_string(),
        }));
    }
}

fn write_catalog(owner_home: &Path, entries: &[(u32, &Path)]) {
    let config = owner_home.join(".config");
    std::fs::create_dir_all(&config).expect("create owner config");
    let contents = entries
        .iter()
        .map(|(number, home)| format!("{number}\t{}\n", home.display()))
        .collect::<String>();
    let path = config.join("codex-accounts.tsv");
    std::fs::write(&path, contents).expect("write owner catalog");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("protect owner catalog");
    }
}

fn write_auth(home: &Path, account_id: &str, token: &str) {
    let encode = |value: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
    let header = encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = encode(
        &serde_json::to_vec(&serde_json::json!({
            "email": "fixture@example.com",
            "email_verified": true,
            "https://api.openai.com/auth": {
                "chatgpt_user_id": "fixture-user",
                "user_id": "fixture-user",
                "chatgpt_plan_type": "pro",
                "chatgpt_account_id": account_id,
            }
        }))
        .expect("encode fixture claims"),
    );
    save_auth(
        home,
        &AuthDotJson {
            auth_mode: Some(AuthMode::Chatgpt),
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: IdTokenInfo {
                    raw_jwt: format!("{header}.{payload}.{}", encode(b"signature")),
                    chatgpt_account_id: Some(account_id.to_string()),
                    ..Default::default()
                },
                access_token: token.to_string(),
                refresh_token: "fixture-refresh".to_string(),
                account_id: Some(account_id.to_string()),
            }),
            last_refresh: Some(chrono::Utc::now()),
            agent_identity: None,
            personal_access_token: None,
            bedrock_api_key: None,
            bedrock_access_keys: None,
        },
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .expect("write fixture auth");
}

fn raw_snapshot(label: &str, source_ref: Option<String>) -> RawSnapshot {
    RawSnapshot {
        label: label.to_string(),
        provider_type: "codex-chatgpt".to_string(),
        source_ref,
        fetched_at: chrono::Utc::now().timestamp(),
        ok: true,
        rate_limit: None,
    }
}

fn subscription_source_ref(account_id: &str, home: &Path) -> Option<String> {
    canonical_subscription_source_ref(account_id, &home.canonicalize().ok()?)
}

async fn registry_fixture(
    process_home: &Path,
    owner_home: &Path,
    token_manager: &MockServer,
) -> AccountRegistry {
    let config = Arc::new(
        ConfigBuilder::default()
            .codex_home(process_home.to_path_buf())
            .build()
            .await
            .expect("build config"),
    );
    let auth_manager = AuthManager::shared_from_managed_auth_config(AuthConfig {
        codex_home: process_home.to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: AuthKeyringBackendKind::default(),
        forced_login_method: None,
        chatgpt_base_url: None,
        forced_chatgpt_workspace_id: None,
        managed_auth_policy: ManagedAuthPolicy::default(),
        auth_route_config: codex_login::test_support::transport_default_auth_route_config(),
    })
    .await;
    let models_manager = codex_core::build_models_manager(config.as_ref(), auth_manager.clone());
    let config_manager = ConfigManager::new(
        process_home.to_path_buf(),
        Vec::new(),
        codex_config::LoaderOverrides::default(),
        /*strict_config*/ false,
        codex_config::CloudConfigBundleLoader::default(),
        codex_arg0::Arg0DispatchPaths::default(),
        Arc::new(codex_config::NoopThreadConfigLoader),
    );
    let mut registry = AccountRegistry::new(
        config,
        config_manager,
        auth_manager,
        models_manager,
        Arc::new(codex_thread_store::InMemoryThreadStore::default()),
    );
    registry.global_directory_user_home = Some(owner_home.to_path_buf());
    registry.token_manager_client = Some(
        TokenManagerClient::new(
            codex_http_client::HttpClientFactory::new(
                codex_http_client::OutboundProxyPolicy::ReqwestDefault,
            ),
            format!("{}/", token_manager.uri())
                .parse()
                .expect("token manager URL"),
        )
        .expect("token manager client"),
    );
    registry
}

fn browser_login(slot_id: &str) -> AccountSlotLoginStartParams {
    AccountSlotLoginStartParams::Chatgpt {
        slot_id: Some(slot_id.to_string()),
        codex_streamlined_login: false,
        use_hosted_login_success_page: false,
        app_brand: None,
    }
}

async fn mount_snapshots(server: &MockServer, accounts: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path("/snapshots"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accounts": accounts,
        })))
        .mount(server)
        .await;
}

async fn logout_params(registry: &AccountRegistry, account_id: &str) -> AccountSlotLogoutParams {
    let listed = registry
        .list(AccountSlotListParams {
            cursor: None,
            limit: None,
        })
        .await
        .expect("list accounts");
    let slot = listed
        .data
        .iter()
        .find(|slot| slot.account_slot_id == account_id)
        .expect("listed account");
    AccountSlotLogoutParams {
        account_slot_id: account_id.to_string(),
        expected_registry_revision: listed.registry_revision,
        expected_attempt_generation: slot.attempt_generation,
    }
}

#[test]
fn global_login_rejects_all_non_exact_browser_inputs() {
    let omitted = AccountSlotLoginStartParams::Chatgpt {
        slot_id: None,
        codex_streamlined_login: false,
        use_hosted_login_success_page: false,
        app_brand: None,
    };
    let api_key = AccountSlotLoginStartParams::ApiKey {
        slot_id: Some("C1".to_string()),
        api_key: "fixture-api-key".to_string(),
    };
    let device_code = AccountSlotLoginStartParams::ChatgptDeviceCode {
        slot_id: Some("C1".to_string()),
    };
    let external_tokens = AccountSlotLoginStartParams::ChatgptAuthTokens {
        slot_id: Some("C1".to_string()),
        access_token: "fixture-access-token".to_string(),
        chatgpt_account_id: "workspace-a".to_string(),
        chatgpt_plan_type: None,
    };

    for params in [omitted, api_key, device_code, external_tokens] {
        assert!(exact_chatgpt_account(&params).is_err());
    }
    assert!(exact_chatgpt_account(&browser_login("legacy-uuid")).is_err());
    assert!(exact_chatgpt_account(&browser_login("C01")).is_err());
}

#[tokio::test]
async fn managed_mode_is_absent_without_catalog_and_fails_closed_on_invalid_authority() {
    let process_home = tempfile::tempdir().expect("process home");
    let other_home = tempfile::tempdir().expect("other home");
    let owner_home = tempfile::tempdir().expect("owner home");
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;

    assert_eq!(registry.global_managed_mode().expect("unmanaged"), false);
    write_catalog(owner_home.path(), &[(1, other_home.path())]);
    assert!(registry.global_managed_mode().is_err());
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    assert_eq!(registry.global_managed_mode().expect("managed"), true);
    std::fs::write(
        owner_home.path().join(".config/codex-accounts.tsv"),
        "malformed\n",
    )
    .expect("malform catalog");
    assert!(registry.global_managed_mode().is_err());
}

#[tokio::test]
async fn global_active_login_projects_exact_operation_and_busy_actions() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    let token_manager = MockServer::start().await;
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    mount_snapshots(&token_manager, Vec::new()).await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;

    assert!(registry.set_global_active_login("C1", "global-login-operation"));
    let active = registry
        .global_slot_snapshot("C1")
        .await
        .expect("active global snapshot");

    assert_eq!(
        active.active_login_operation_id.as_deref(),
        Some("global-login-operation")
    );
    assert!(active.actions.iter().all(|action| !action.allowed));
    assert_eq!(
        active
            .actions
            .iter()
            .map(|action| action.action)
            .collect::<Vec<_>>(),
        vec![
            AccountSlotAction::Login,
            AccountSlotAction::RetryLogin,
            AccountSlotAction::SwitchTo,
            AccountSlotAction::Logout,
        ]
    );

    registry.clear_global_active_login("C1", "global-login-operation");
    let cleared = registry
        .global_slot_snapshot("C1")
        .await
        .expect("cleared global snapshot");
    assert_eq!(cleared.active_login_operation_id, None);
    assert!(
        cleared
            .actions
            .iter()
            .any(|action| action.action == AccountSlotAction::Login && action.allowed)
    );
}

#[tokio::test]
async fn unknown_registered_account_is_rejected_before_writer_gate() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    let authority = FixtureAuthority::default();

    assert!(
        registry
            .begin_global_login_with(&browser_login("C2"), &authority)
            .await
            .is_err()
    );
    assert_eq!(authority.actions(), Vec::<Value>::new());
}

#[tokio::test]
async fn logged_out_registered_account_commits_first_identity_and_snapshot() {
    let process_home = tempfile::tempdir().expect("process home");
    let c2 = tempfile::tempdir().expect("C2 home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(
        owner_home.path(),
        &[(1, process_home.path()), (2, c2.path())],
    );
    let token_manager = MockServer::start().await;
    let source_ref = subscription_source_ref("workspace-new", c2.path()).expect("source ref");
    mount_snapshots(
        &token_manager,
        vec![serde_json::json!({
            "label": "C2", "type": "codex-chatgpt", "sourceRef": source_ref,
            "fetchedAt": chrono::Utc::now().timestamp(), "ok": true
        })],
    )
    .await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    registry
        .global_catalog
        .replace(vec![raw_snapshot("C2", None)])
        .expect("seed catalog");
    let authority = FixtureAuthority::default();

    let lifecycle = registry
        .begin_global_login_with(&browser_login("C2"), &authority)
        .await
        .expect("begin login");
    let staging_home = lifecycle.staging_home().to_path_buf();
    write_auth(&staging_home, "workspace-new", "candidate-token");
    let state = registry
        .commit_global_login(lifecycle)
        .await
        .expect("commit login");

    assert_eq!(state.account_id(), "workspace-new");
    assert!(!staging_home.exists());
    assert_eq!(
        authority.actions(),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C2"}),
            serde_json::json!({"method": "lifecycle/commit", "accountId": "C2"}),
        ]
    );
}

#[tokio::test]
async fn conflicting_first_identity_aborts_before_persist() {
    let process_home = tempfile::tempdir().expect("process home");
    let c2 = tempfile::tempdir().expect("C2 home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(
        owner_home.path(),
        &[(1, process_home.path()), (2, c2.path())],
    );
    write_auth(process_home.path(), "workspace-shared", "current-token");
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    let authority = FixtureAuthority::default();
    let lifecycle = registry
        .begin_global_login_with(&browser_login("C2"), &authority)
        .await
        .expect("begin login");
    write_auth(
        lifecycle.staging_home(),
        "workspace-shared",
        "candidate-token",
    );

    assert!(registry.commit_global_login(lifecycle).await.is_err());
    assert!(managed_auth_state(c2.path()).expect("read C2").is_none());
    assert_eq!(
        authority.actions(),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C2"}),
            serde_json::json!({"method": "lifecycle/abort", "accountId": "C2"}),
        ]
    );
}

#[tokio::test]
async fn identity_mismatch_aborts_and_preserves_existing_credential() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    write_auth(process_home.path(), "workspace-a", "current-token");
    let original = std::fs::read(process_home.path().join("auth.json")).expect("read original");
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    let source_ref =
        subscription_source_ref("workspace-a", process_home.path()).expect("source ref");
    registry
        .global_catalog
        .replace(vec![raw_snapshot("C1", Some(source_ref))])
        .expect("seed catalog");
    let authority = FixtureAuthority::default();

    let lifecycle = registry
        .begin_global_login_with(&browser_login("C1"), &authority)
        .await
        .expect("begin reauth");
    let staging_home = lifecycle.staging_home().to_path_buf();
    write_auth(&staging_home, "workspace-b", "candidate-token");

    assert!(registry.commit_global_login(lifecycle).await.is_err());
    assert_eq!(
        std::fs::read(process_home.path().join("auth.json")).expect("read preserved"),
        original
    );
    assert!(!staging_home.exists());
    assert_eq!(
        authority.actions(),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/abort", "accountId": "C1"}),
        ]
    );
}

#[tokio::test]
async fn explicit_cancel_aborts_and_cleans_staging() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    let authority = FixtureAuthority::default();

    let lifecycle = registry
        .begin_global_login_with(&browser_login("C1"), &authority)
        .await
        .expect("begin login");
    let staging_home = lifecycle.staging_home().to_path_buf();
    registry.abort_global_login(lifecycle).await;

    assert!(!staging_home.exists());
    assert_eq!(
        authority.actions(),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/abort", "accountId": "C1"}),
        ]
    );
}

#[tokio::test]
async fn lifecycle_serializes_same_account_while_other_accounts_remain_independent() {
    let process_home = tempfile::tempdir().expect("process home");
    let c2 = tempfile::tempdir().expect("C2 home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(
        owner_home.path(),
        &[(1, process_home.path()), (2, c2.path())],
    );
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    let authority = FixtureAuthority::default();

    let first = registry
        .begin_global_login_with(&browser_login("C1"), &authority)
        .await
        .expect("begin C1");
    assert!(
        registry
            .begin_global_login_with(&browser_login("C1"), &authority)
            .await
            .is_err()
    );
    let other = registry
        .begin_global_login_with(&browser_login("C2"), &authority)
        .await
        .expect("begin C2");
    registry.abort_global_login(first).await;
    registry.abort_global_login(other).await;

    assert_eq!(
        authority.actions(),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C2"}),
            serde_json::json!({"method": "lifecycle/abort", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/abort", "accountId": "C2"}),
        ]
    );
}

#[tokio::test]
async fn concurrent_first_logins_persist_a_shared_identity_for_only_one_account() {
    let process_home = tempfile::tempdir().expect("process home");
    let c2 = tempfile::tempdir().expect("C2 home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(
        owner_home.path(),
        &[(1, process_home.path()), (2, c2.path())],
    );
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    registry
        .global_catalog
        .replace(vec![raw_snapshot("C1", None), raw_snapshot("C2", None)])
        .expect("seed catalog");
    let authority = FixtureAuthority::default();
    let first = registry
        .begin_global_login_with(&browser_login("C1"), &authority)
        .await
        .expect("begin C1");
    let second = registry
        .begin_global_login_with(&browser_login("C2"), &authority)
        .await
        .expect("begin C2");
    write_auth(first.staging_home(), "workspace-shared", "first-token");
    write_auth(second.staging_home(), "workspace-shared", "second-token");

    let (first_result, second_result) = tokio::join!(
        registry.commit_global_login(first),
        registry.commit_global_login(second)
    );

    assert!(first_result.is_err());
    assert!(second_result.is_err());
    let persisted = [process_home.path(), c2.path()]
        .into_iter()
        .filter(|home| managed_auth_state(home).expect("read credential").is_some())
        .count();
    assert_eq!(persisted, 1);
    let actions = authority.actions();
    assert_eq!(
        actions
            .iter()
            .filter(|action| action["method"] == "lifecycle/commit")
            .count(),
        1
    );
    assert_eq!(
        actions
            .iter()
            .filter(|action| action["method"] == "lifecycle/abort")
            .count(),
        1
    );
}

#[tokio::test]
async fn post_commit_readiness_failure_preserves_promoted_credential() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    let token_manager = MockServer::start().await;
    mount_snapshots(
        &token_manager,
        vec![serde_json::json!({
            "label": "C1", "type": "codex-chatgpt", "sourceRef": null,
            "fetchedAt": chrono::Utc::now().timestamp(), "ok": false
        })],
    )
    .await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    registry
        .global_catalog
        .replace(vec![raw_snapshot("C1", None)])
        .expect("seed catalog");
    let authority = FixtureAuthority::default();
    let lifecycle = registry
        .begin_global_login_with(&browser_login("C1"), &authority)
        .await
        .expect("begin login");
    write_auth(lifecycle.staging_home(), "workspace-new", "candidate-token");

    assert!(registry.commit_global_login(lifecycle).await.is_err());
    assert_eq!(
        managed_auth_state(process_home.path())
            .expect("read committed")
            .expect("committed credential")
            .account_id(),
        "workspace-new"
    );
    assert_eq!(
        authority.actions(),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/commit", "accountId": "C1"}),
        ]
    );
}

#[tokio::test]
async fn commit_response_loss_preserves_promoted_credential() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    registry
        .global_catalog
        .replace(vec![raw_snapshot("C1", None)])
        .expect("seed catalog");
    let authority = FixtureAuthority::failing_commit();
    let lifecycle = registry
        .begin_global_login_with(&browser_login("C1"), &authority)
        .await
        .expect("begin login");
    write_auth(lifecycle.staging_home(), "workspace-new", "candidate-token");

    let error = registry
        .commit_global_login(lifecycle)
        .await
        .expect_err("lost commit response should report unavailable");
    assert_eq!(error.message, LIFECYCLE_UNAVAILABLE);
    assert_eq!(
        managed_auth_state(process_home.path())
            .expect("read committed")
            .expect("committed credential")
            .account_id(),
        "workspace-new"
    );
}

#[tokio::test]
async fn logout_uses_snapshot_cas_and_observes_absent_identity() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    write_auth(process_home.path(), "workspace-a", "current-token");
    let token_manager = MockServer::start().await;
    mount_snapshots(
        &token_manager,
        vec![serde_json::json!({
            "label": "C1", "type": "codex-chatgpt", "sourceRef": null,
            "fetchedAt": chrono::Utc::now().timestamp(), "ok": false
        })],
    )
    .await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    let source_ref =
        subscription_source_ref("workspace-a", process_home.path()).expect("source ref");
    registry
        .global_catalog
        .replace(vec![raw_snapshot("C1", Some(source_ref))])
        .expect("seed catalog");
    let params = logout_params(&registry, "C1").await;
    let authority = FixtureAuthority::default();

    registry
        .logout_global_account_with(&params, &authority)
        .await
        .expect("logout");

    assert!(
        managed_auth_state(process_home.path())
            .expect("read auth")
            .is_none()
    );
    assert_eq!(
        authority.actions(),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/commit", "accountId": "C1"}),
        ]
    );
}

#[tokio::test]
async fn stale_logout_never_enters_writer_gate_or_deletes_credential() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    write_auth(process_home.path(), "workspace-a", "current-token");
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    let source_ref =
        subscription_source_ref("workspace-a", process_home.path()).expect("source ref");
    registry
        .global_catalog
        .replace(vec![raw_snapshot("C1", Some(source_ref))])
        .expect("seed catalog");
    let mut params = logout_params(&registry, "C1").await;
    params.expected_registry_revision = params.expected_registry_revision.saturating_add(1);
    let authority = FixtureAuthority::default();

    assert!(
        registry
            .logout_global_account_with(&params, &authority)
            .await
            .is_err()
    );
    assert!(
        managed_auth_state(process_home.path())
            .expect("read credential")
            .is_some()
    );
    assert_eq!(authority.actions(), Vec::<Value>::new());
}

#[tokio::test]
async fn post_commit_logout_readiness_failure_preserves_deletion() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    write_auth(process_home.path(), "workspace-a", "current-token");
    let token_manager = MockServer::start().await;
    let source_ref =
        subscription_source_ref("workspace-a", process_home.path()).expect("source ref");
    mount_snapshots(
        &token_manager,
        vec![serde_json::json!({
            "label": "C1", "type": "codex-chatgpt", "sourceRef": source_ref,
            "fetchedAt": chrono::Utc::now().timestamp(), "ok": true
        })],
    )
    .await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    registry
        .global_catalog
        .replace(vec![raw_snapshot("C1", Some(source_ref))])
        .expect("seed catalog");
    let params = logout_params(&registry, "C1").await;
    let authority = FixtureAuthority::default();

    assert!(
        registry
            .logout_global_account_with(&params, &authority)
            .await
            .is_err()
    );
    assert!(
        managed_auth_state(process_home.path())
            .expect("read credential")
            .is_none()
    );
    assert_eq!(
        authority.actions(),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/commit", "accountId": "C1"}),
        ]
    );
}

#[tokio::test]
async fn logout_commit_response_loss_preserves_deletion() {
    let process_home = tempfile::tempdir().expect("process home");
    let owner_home = tempfile::tempdir().expect("owner home");
    write_catalog(owner_home.path(), &[(1, process_home.path())]);
    write_auth(process_home.path(), "workspace-a", "current-token");
    let token_manager = MockServer::start().await;
    let registry = registry_fixture(process_home.path(), owner_home.path(), &token_manager).await;
    let source_ref =
        subscription_source_ref("workspace-a", process_home.path()).expect("source ref");
    registry
        .global_catalog
        .replace(vec![raw_snapshot("C1", Some(source_ref))])
        .expect("seed catalog");
    let params = logout_params(&registry, "C1").await;
    let authority = FixtureAuthority::failing_commit();

    let error = registry
        .logout_global_account_with(&params, &authority)
        .await
        .expect_err("lost commit response should report unavailable");
    assert_eq!(error.message, LIFECYCLE_UNAVAILABLE);
    assert!(
        managed_auth_state(process_home.path())
            .expect("read credential")
            .is_none()
    );
    assert_eq!(
        authority.actions(),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/commit", "accountId": "C1"}),
        ]
    );
}
