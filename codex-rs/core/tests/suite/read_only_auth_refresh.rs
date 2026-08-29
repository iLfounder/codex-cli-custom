use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use base64::Engine;
use codex_config::ManagedAuthPolicy;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::execution_account::ExecutionAccountContext;
use codex_core::execution_account::ExecutionAccountResolver;
use codex_core::execution_account::ExecutionAccountResolverFuture;
use codex_login::AuthConfig;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::ExternalAuthFuture;
use codex_login::auth::ReadOnlyAuthRefresh;
use codex_login::save_auth;
use codex_login::token_data::IdTokenInfo;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::protocol::ExecutionAccountBinding;
use core_test_support::responses;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

const SLOT_ID: &str = "C4";
const ACCOUNT_ID: &str = "account-read-only";
const INITIAL_ACCESS_TOKEN: &str = "header.e30.initial";
const REFRESHED_ACCESS_TOKEN: &str = "header.e30.refreshed";

struct SingleAccountResolver {
    context: Arc<ExecutionAccountContext>,
}

impl ExecutionAccountResolver for SingleAccountResolver {
    fn initial_binding_for_new_thread(&self) -> ExecutionAccountBinding {
        self.context.binding.clone()
    }

    fn resolve(&self, binding: ExecutionAccountBinding) -> ExecutionAccountResolverFuture<'_> {
        let context = Arc::clone(&self.context);
        Box::pin(async move {
            if binding != context.binding {
                return Err(codex_protocol::error::CodexErr::InvalidRequest(
                    "test account binding changed".to_string(),
                ));
            }
            Ok(context)
        })
    }
}

struct FileRefreshAuthority {
    auth_home: PathBuf,
    calls: AtomicUsize,
}

impl ReadOnlyAuthRefresh for FileRefreshAuthority {
    fn force_refresh(&self) -> ExternalAuthFuture<'_, ()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = write_auth(&self.auth_home, REFRESHED_ACCESS_TOKEN);
        Box::pin(async move { result })
    }
}

fn write_auth(auth_home: &Path, access_token: &str) -> std::io::Result<()> {
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
            "email": "read-only@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_user_id": "user-read-only",
                "user_id": "user-read-only",
                "chatgpt_plan_type": "pro",
                "chatgpt_account_id": ACCOUNT_ID,
            },
        }))?);
    let id_token = format!("eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.{payload}.c2ln");
    save_auth(
        auth_home,
        &AuthDotJson {
            auth_mode: None,
            openai_api_key: None,
            tokens: Some(codex_login::TokenData {
                id_token: IdTokenInfo {
                    raw_jwt: id_token,
                    ..Default::default()
                },
                access_token: access_token.to_string(),
                refresh_token: "test-refresh-token".to_string(),
                account_id: Some(ACCOUNT_ID.to_string()),
            }),
            last_refresh: Some(chrono::Utc::now()),
            agent_identity: None,
            personal_access_token: None,
            bedrock_api_key: None,
        },
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
}

async fn build_read_only_test(
    server: &MockServer,
) -> anyhow::Result<(TestCodex, Arc<FileRefreshAuthority>)> {
    let home = Arc::new(TempDir::new()?);
    let auth_home = home.path().join("account-C4");
    std::fs::create_dir(&auth_home)?;
    write_auth(&auth_home, INITIAL_ACCESS_TOKEN)?;
    let refresh = Arc::new(FileRefreshAuthority {
        auth_home: auth_home.clone(),
        calls: AtomicUsize::new(0),
    });
    let auth_manager = AuthManager::shared_from_read_only_auth_config_with_refresh(
        AuthConfig {
            codex_home: auth_home.clone(),
            auth_credentials_store_mode: AuthCredentialsStoreMode::File,
            keyring_backend_kind: AuthKeyringBackendKind::default(),
            forced_login_method: None,
            chatgpt_base_url: None,
            forced_chatgpt_workspace_id: None,
            managed_auth_policy: ManagedAuthPolicy::default(),
            auth_route_config: codex_login::test_support::transport_default_auth_route_config(),
        },
        refresh.clone(),
    )
    .await?;
    let provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        supports_websockets: false,
        ..built_in_model_providers(/*openai_base_url*/ None)["openai"].clone()
    };
    let models_manager: SharedModelsManager =
        codex_core::test_support::models_manager_with_provider(
            auth_home,
            Arc::clone(&auth_manager),
            provider,
        );
    let binding = ExecutionAccountBinding {
        slot_id: SLOT_ID.to_string(),
        generation: 1,
    };
    let resolver = Arc::new(SingleAccountResolver {
        context: Arc::new(ExecutionAccountContext {
            binding,
            auth_manager,
            models_manager: models_manager.clone(),
        }),
    });
    let test = test_codex()
        .with_home(home)
        .with_models_manager(models_manager)
        .with_execution_account_resolver(resolver)
        .build_with_auto_env(server)
        .await?;
    Ok((test, refresh))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_401_refreshes_read_only_auth_and_retries_once() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = responses::mount_response_sequence(
        &server,
        vec![
            ResponseTemplate::new(401),
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse(vec![
                    responses::ev_response_created("response-refreshed"),
                    responses::ev_completed("response-refreshed"),
                ])),
        ],
    )
    .await;
    let (test, refresh) = build_read_only_test(&server).await?;

    test.submit_turn("retry with refreshed credentials").await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.header("authorization"))
            .collect::<Vec<_>>(),
        vec![
            Some(format!("Bearer {INITIAL_ACCESS_TOKEN}")),
            Some(format!("Bearer {REFRESHED_ACCESS_TOKEN}")),
        ]
    );
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_401_is_final_without_another_refresh_or_request() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = responses::mount_response_sequence(
        &server,
        vec![ResponseTemplate::new(401), ResponseTemplate::new(401)],
    )
    .await;
    let (test, refresh) = build_read_only_test(&server).await?;

    test.submit_turn("stop after the bounded retry").await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 1);
    Ok(())
}
