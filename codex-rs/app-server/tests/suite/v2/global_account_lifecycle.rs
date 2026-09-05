use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use app_test_support::ChatGptIdTokenClaims;
use app_test_support::TestAppServer;
use app_test_support::encode_id_token;
use base64::Engine;
use codex_app_server_protocol::AccountSlotListParams;
use codex_app_server_protocol::AccountSlotListResponse;
use codex_app_server_protocol::AccountSlotLoginChallenge;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_app_server_protocol::AccountSlotLogoutResponse;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::CancelLoginAccountParams;
use codex_app_server_protocol::CancelLoginAccountResponse;
use codex_app_server_protocol::CancelLoginAccountStatus;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeOperationUpdatedNotification;
use codex_http_client::HttpClientBuilder;
use pretty_assertions::assert_eq;
use serial_test::serial;
use sha2::Digest;
use sha2::Sha256;
use tempfile::TempDir;
use url::Url;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
#[serial(login_port)]
async fn exact_c_browser_cancel_aborts_connection_owned_lifecycle() -> Result<()> {
    let owner_home = TempDir::new()?;
    let account_home = owner_home.path().join("account1");
    std::fs::create_dir_all(&account_home)?;
    write_owner_catalog(owner_home.path(), &account_home)?;
    std::fs::write(
        account_home.join("config.toml"),
        "model = \"mock-model\"\ncli_auth_credentials_store = \"file\"\n",
    )?;

    let token_manager = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshots"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accounts": [],
        })))
        .mount(&token_manager)
        .await;
    let (control_rx, control_thread) = start_control_fixture(owner_home.path())?;
    let endpoint = format!("{}/", token_manager.uri());
    let owner_home_env = owner_home.path().to_string_lossy();
    let mut app = TestAppServer::builder()
        .with_codex_home(&account_home)
        .without_auto_env()
        .with_env_overrides(&[
            ("HOME", Some(owner_home_env.as_ref())),
            ("CODEX_TEST_OWNER_HOME", Some(owner_home_env.as_ref())),
            (
                "CODEX_APP_SERVER_TEST_TOKEN_MANAGER_URL",
                Some(endpoint.as_str()),
            ),
            ("CODEX_API_KEY", None),
            ("CODEX_ACCESS_TOKEN", None),
            ("OPENAI_API_KEY", None),
        ])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = app
        .send_raw_request(
            "accountSlot/login/start",
            Some(serde_json::to_value(
                AccountSlotLoginStartParams::Chatgpt {
                    slot_id: Some("C1".to_string()),
                    codex_streamlined_login: false,
                    use_hosted_login_success_page: false,
                    app_brand: None,
                },
            )?),
        )
        .await?;
    let started: AccountSlotLoginStartResponse = app.read_response(request_id).await?;
    let Some(AccountSlotLoginChallenge::Browser { login_id, .. }) = started.challenge else {
        anyhow::bail!("expected exact-C browser challenge");
    };
    assert_eq!(
        started.slot.active_login_operation_id.as_deref(),
        Some(login_id.as_str())
    );
    assert!(started.slot.actions.iter().all(|action| !action.allowed));

    let cancel_id = app
        .send_cancel_login_account_request(CancelLoginAccountParams { login_id })
        .await?;
    let canceled: CancelLoginAccountResponse = app.read_response(cancel_id).await?;
    assert_eq!(canceled.status, CancelLoginAccountStatus::Canceled);
    assert_eq!(control_rx.recv_timeout(DEFAULT_TIMEOUT)?, "lifecycle/begin");
    assert_eq!(control_rx.recv_timeout(DEFAULT_TIMEOUT)?, "lifecycle/abort");
    control_thread
        .join()
        .map_err(|_| anyhow::anyhow!("control fixture panicked"))?;
    assert!(!account_home.join("auth.json").exists());

    let direct_login = app
        .send_login_account_api_key_request("must-not-be-persisted")
        .await?;
    let direct_login_error = app
        .read_stream_until_error_message(RequestId::Integer(direct_login))
        .await?;
    assert_eq!(direct_login_error.error.code, -32600);
    let direct_logout = app.send_logout_account_request().await?;
    let direct_logout_error = app
        .read_stream_until_error_message(RequestId::Integer(direct_logout))
        .await?;
    assert_eq!(direct_logout_error.error.code, -32600);
    assert!(!account_home.join("auth.json").exists());
    Ok(())
}

#[tokio::test]
#[serial(login_port)]
async fn exact_c_disconnect_aborts_connection_owned_lifecycle() -> Result<()> {
    let owner_home = TempDir::new()?;
    let account_home = owner_home.path().join("account1");
    std::fs::create_dir_all(&account_home)?;
    write_owner_catalog(owner_home.path(), &account_home)?;
    write_account_config(&account_home)?;

    let token_manager = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshots"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accounts": [],
        })))
        .mount(&token_manager)
        .await;
    let (control_rx, control_thread) = start_control_fixture(owner_home.path())?;
    let endpoint = format!("{}/", token_manager.uri());
    let owner_home_env = owner_home.path().to_string_lossy();
    let mut app = TestAppServer::builder()
        .with_codex_home(&account_home)
        .without_auto_env()
        .with_env_overrides(&[
            ("HOME", Some(owner_home_env.as_ref())),
            ("CODEX_TEST_OWNER_HOME", Some(owner_home_env.as_ref())),
            (
                "CODEX_APP_SERVER_TEST_TOKEN_MANAGER_URL",
                Some(endpoint.as_str()),
            ),
            ("CODEX_API_KEY", None),
            ("CODEX_ACCESS_TOKEN", None),
            ("OPENAI_API_KEY", None),
        ])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let started = start_exact_c_login(&mut app).await?;
    assert!(matches!(
        started.challenge,
        Some(AccountSlotLoginChallenge::Browser { .. })
    ));
    tokio::time::timeout(DEFAULT_TIMEOUT, app.shutdown_gracefully()).await??;

    assert_eq!(control_rx.recv_timeout(DEFAULT_TIMEOUT)?, "lifecycle/begin");
    assert_eq!(control_rx.recv_timeout(DEFAULT_TIMEOUT)?, "lifecycle/abort");
    control_thread
        .join()
        .map_err(|_| anyhow::anyhow!("control fixture panicked"))?;
    assert!(!account_home.join("auth.json").exists());
    Ok(())
}

#[tokio::test]
#[serial(login_port)]
async fn exact_c_login_reauth_and_logout_commit_matching_snapshots() -> Result<()> {
    let owner_home = TempDir::new()?;
    let account_home = owner_home.path().join("account1");
    std::fs::create_dir_all(&account_home)?;
    write_owner_catalog(owner_home.path(), &account_home)?;
    write_account_config(&account_home)?;
    let source_ref = subscription_source_ref("workspace-c1", &account_home);
    let ready_snapshot = vec![managed_snapshot("C1", &source_ref)];
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let token_manager = start_token_manager_http(Arc::clone(&snapshots)).await;
    let oauth = start_oauth_fixture("workspace-c1").await?;
    let plans = vec![
        ControlPlan::commit("absent", ready_snapshot.clone()),
        ControlPlan::commit("active", ready_snapshot),
        ControlPlan::commit("active", Vec::new()),
    ];
    let (control_rx, control_thread) =
        start_control_sequence(owner_home.path(), Arc::clone(&snapshots), plans)?;
    let mut app =
        start_managed_app(&account_home, owner_home.path(), &token_manager, &oauth).await?;

    let first = start_exact_c_login(&mut app).await?;
    complete_browser_callback(first.challenge.as_ref(), &oauth).await?;
    wait_for_operation(
        &mut app,
        &first.operation.operation_id,
        SessionRuntimeOperationStatus::Ready,
    )
    .await?;
    let first_ready = wait_for_inventory(&mut app).await?;
    let listed = list_accounts(&mut app).await?;
    assert_eq!(listed.data[0].status, AccountSlotStatus::Ready);
    assert_eq!(listed.registry_revision, first_ready);

    let reauth = start_exact_c_login(&mut app).await?;
    complete_browser_callback(reauth.challenge.as_ref(), &oauth).await?;
    wait_for_operation(
        &mut app,
        &reauth.operation.operation_id,
        SessionRuntimeOperationStatus::Ready,
    )
    .await?;
    let reauth_revision = wait_for_inventory(&mut app).await?;
    assert!(reauth_revision > first_ready);

    let listed = list_accounts(&mut app).await?;
    let slot = listed
        .data
        .iter()
        .find(|slot| slot.account_slot_id == "C1")
        .unwrap();
    let request_id = app
        .send_raw_request(
            "accountSlot/logout",
            Some(serde_json::to_value(AccountSlotLogoutParams {
                account_slot_id: "C1".to_string(),
                expected_registry_revision: listed.registry_revision,
                expected_attempt_generation: slot.attempt_generation,
            })?),
        )
        .await?;
    let logged_out: AccountSlotLogoutResponse = app.read_response(request_id).await?;
    assert_eq!(logged_out.slot.status, AccountSlotStatus::LoginRequired);
    assert!(wait_for_inventory(&mut app).await? > reauth_revision);
    assert!(!account_home.join("auth.json").exists());

    assert_eq!(
        (0..6)
            .map(|_| control_rx.recv_timeout(DEFAULT_TIMEOUT))
            .collect::<Result<Vec<_>, _>>()?,
        vec![
            "lifecycle/begin",
            "lifecycle/commit",
            "lifecycle/begin",
            "lifecycle/commit",
            "lifecycle/begin",
            "lifecycle/commit",
        ]
    );
    control_thread
        .join()
        .map_err(|_| anyhow::anyhow!("control fixture panicked"))?;
    Ok(())
}

#[tokio::test]
#[serial(login_port)]
async fn exact_c_post_commit_readiness_failure_keeps_committed_credential() -> Result<()> {
    let owner_home = TempDir::new()?;
    let account_home = owner_home.path().join("account1");
    std::fs::create_dir_all(&account_home)?;
    write_owner_catalog(owner_home.path(), &account_home)?;
    write_account_config(&account_home)?;
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let token_manager = start_token_manager_http(Arc::clone(&snapshots)).await;
    let oauth = start_oauth_fixture("workspace-c1").await?;
    let (control_rx, control_thread) = start_control_sequence(
        owner_home.path(),
        Arc::clone(&snapshots),
        vec![ControlPlan::commit("absent", Vec::new())],
    )?;
    let mut app =
        start_managed_app(&account_home, owner_home.path(), &token_manager, &oauth).await?;

    let started = start_exact_c_login(&mut app).await?;
    complete_browser_callback(started.challenge.as_ref(), &oauth).await?;
    let failed = wait_for_operation(
        &mut app,
        &started.operation.operation_id,
        SessionRuntimeOperationStatus::Failed,
    )
    .await?;
    assert!(
        failed
            .operation
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("committed but is not ready"))
    );
    assert!(account_home.join("auth.json").exists());
    assert_eq!(
        [
            control_rx.recv_timeout(DEFAULT_TIMEOUT)?,
            control_rx.recv_timeout(DEFAULT_TIMEOUT)?,
        ],
        ["lifecycle/begin", "lifecycle/commit"]
    );
    control_thread
        .join()
        .map_err(|_| anyhow::anyhow!("control fixture panicked"))?;
    Ok(())
}

#[derive(Clone)]
struct ControlPlan {
    begin_state: &'static str,
    finish_method: &'static str,
    snapshots_after_finish: Vec<serde_json::Value>,
}

impl ControlPlan {
    fn commit(begin_state: &'static str, snapshots_after_finish: Vec<serde_json::Value>) -> Self {
        Self {
            begin_state,
            finish_method: "lifecycle/commit",
            snapshots_after_finish,
        }
    }
}

async fn start_managed_app(
    account_home: &std::path::Path,
    owner_home: &std::path::Path,
    token_manager: &MockServer,
    oauth: &MockServer,
) -> Result<TestAppServer> {
    let endpoint = format!("{}/", token_manager.uri());
    let owner_home_env = owner_home.to_string_lossy();
    TestAppServer::builder()
        .with_codex_home(account_home)
        .without_auto_env()
        .with_env_overrides(&[
            ("HOME", Some(owner_home_env.as_ref())),
            ("CODEX_TEST_OWNER_HOME", Some(owner_home_env.as_ref())),
            (
                "CODEX_APP_SERVER_TEST_TOKEN_MANAGER_URL",
                Some(endpoint.as_str()),
            ),
            (
                "CODEX_APP_SERVER_TEST_OAUTH_ISSUER",
                Some(oauth.uri().as_str()),
            ),
            ("CODEX_API_KEY", None),
            ("CODEX_ACCESS_TOKEN", None),
            ("OPENAI_API_KEY", None),
        ])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await
}

async fn start_token_manager_http(snapshots: Arc<Mutex<Vec<serde_json::Value>>>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshots"))
        .respond_with(move |_request: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accounts": snapshots.lock().expect("snapshot state").clone(),
            }))
        })
        .mount(&server)
        .await;
    server
}

async fn start_oauth_fixture(account_id: &str) -> Result<MockServer> {
    let server = MockServer::start().await;
    let id_token = encode_id_token(
        &ChatGptIdTokenClaims::new()
            .email("fixture@example.test")
            .plan_type("pro")
            .chatgpt_account_id(account_id),
    )?;
    let sequence = Arc::new(AtomicUsize::new(1));
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(move |request: &wiremock::Request| {
            if String::from_utf8_lossy(&request.body).contains("token-exchange") {
                return ResponseTemplate::new(400);
            }
            let sequence = sequence.fetch_add(1, Ordering::Relaxed);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id_token": id_token,
                "access_token": id_token,
                "refresh_token": format!("fixture-refresh-{sequence}"),
            }))
        })
        .mount(&server)
        .await;
    Ok(server)
}

fn managed_snapshot(label: &str, source_ref: &str) -> serde_json::Value {
    serde_json::json!({
        "label": label,
        "type": "codex-chatgpt",
        "sourceRef": source_ref,
        "fetchedAt": chrono::Utc::now().timestamp(),
        "ok": true,
        "rateLimit": null,
    })
}

fn subscription_source_ref(account_id: &str, account_home: &std::path::Path) -> String {
    let account_home = account_home
        .canonicalize()
        .expect("canonical managed account home");
    let mut digest = Sha256::new();
    digest.update(b"llm-bridge.subscription-source-ref/v1\0codex-cli\0");
    digest.update(account_id.as_bytes());
    digest.update(b"\0");
    digest.update(account_home.to_string_lossy().as_bytes());
    format!(
        "subscription-source-v1:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.finalize())
    )
}

fn write_account_config(account_home: &std::path::Path) -> Result<()> {
    std::fs::write(
        account_home.join("config.toml"),
        "model = \"mock-model\"\ncli_auth_credentials_store = \"file\"\n",
    )?;
    Ok(())
}

async fn start_exact_c_login(app: &mut TestAppServer) -> Result<AccountSlotLoginStartResponse> {
    let request_id = app
        .send_raw_request(
            "accountSlot/login/start",
            Some(serde_json::to_value(
                AccountSlotLoginStartParams::Chatgpt {
                    slot_id: Some("C1".to_string()),
                    codex_streamlined_login: false,
                    use_hosted_login_success_page: false,
                    app_brand: None,
                },
            )?),
        )
        .await?;
    app.read_response(request_id).await
}

async fn complete_browser_callback(
    challenge: Option<&AccountSlotLoginChallenge>,
    oauth: &MockServer,
) -> Result<()> {
    let Some(AccountSlotLoginChallenge::Browser { auth_url, .. }) = challenge else {
        anyhow::bail!("expected browser challenge");
    };
    let auth_url = Url::parse(auth_url)?;
    assert_eq!(auth_url.origin().ascii_serialization(), oauth.uri());
    let redirect_uri = auth_url
        .query_pairs()
        .find_map(|(key, value)| (key == "redirect_uri").then(|| value.into_owned()))
        .ok_or_else(|| anyhow::anyhow!("missing redirect URI"))?;
    let state = auth_url
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .ok_or_else(|| anyhow::anyhow!("missing callback state"))?;
    let mut callback = Url::parse(&redirect_uri)?;
    callback
        .query_pairs_mut()
        .append_pair("code", "fixture-code")
        .append_pair("state", &state);
    let response = HttpClientBuilder::new()
        .build_direct()?
        .get(callback)
        .send()
        .await?;
    assert!(response.status().is_success());
    Ok(())
}

async fn wait_for_operation(
    app: &mut TestAppServer,
    operation_id: &str,
    status: SessionRuntimeOperationStatus,
) -> Result<SessionRuntimeOperationUpdatedNotification> {
    loop {
        let update: SessionRuntimeOperationUpdatedNotification = tokio::time::timeout(
            DEFAULT_TIMEOUT,
            app.read_notification("sessionRuntime/operation/updated"),
        )
        .await??;
        if update.operation.operation_id == operation_id && update.operation.status == status {
            return Ok(update);
        }
    }
}

async fn list_accounts(app: &mut TestAppServer) -> Result<AccountSlotListResponse> {
    let request_id = app
        .send_raw_request(
            "accountSlot/list",
            Some(serde_json::to_value(AccountSlotListParams {
                cursor: None,
                limit: None,
            })?),
        )
        .await?;
    app.read_response(request_id).await
}

async fn wait_for_inventory(app: &mut TestAppServer) -> Result<u64> {
    let notification: codex_app_server_protocol::AccountSlotInventoryChangedNotification =
        tokio::time::timeout(
            DEFAULT_TIMEOUT,
            app.read_notification("accountSlot/inventoryChanged"),
        )
        .await??;
    Ok(notification.registry_revision)
}

fn start_control_sequence(
    owner_home: &std::path::Path,
    snapshots: Arc<Mutex<Vec<serde_json::Value>>>,
    plans: Vec<ControlPlan>,
) -> Result<(mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let control_dir = owner_home.join(".tokenmanager/control");
    std::fs::create_dir_all(&control_dir)?;
    std::fs::set_permissions(&control_dir, std::fs::Permissions::from_mode(0o700))?;
    let listener = UnixListener::bind(control_dir.join("tokenmanager.sock"))?;
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        for plan in plans {
            let (mut stream, _) = listener.accept().expect("accept control connection");
            let mut reader = BufReader::new(stream.try_clone().expect("clone control stream"));
            for (method, state) in [
                ("lifecycle/begin", plan.begin_state),
                (plan.finish_method, "committed"),
            ] {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read control request");
                let request: serde_json::Value = serde_json::from_str(&line).expect("control JSON");
                assert_eq!(request["method"], method);
                tx.send(method.to_string()).expect("send observed method");
                if method == plan.finish_method {
                    *snapshots.lock().expect("snapshot state") =
                        plan.snapshots_after_finish.clone();
                }
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({"ok": true, "state": state, "generation": 1})
                )
                .expect("write control response");
            }
        }
    });
    Ok((rx, handle))
}

fn write_owner_catalog(owner_home: &std::path::Path, account_home: &std::path::Path) -> Result<()> {
    let config = owner_home.join(".config");
    std::fs::create_dir_all(&config)?;
    let catalog = config.join("codex-accounts.tsv");
    std::fs::write(&catalog, format!("1\t{}\n", account_home.display()))?;
    std::fs::set_permissions(&catalog, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn start_control_fixture(
    owner_home: &std::path::Path,
) -> Result<(mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let control_dir = owner_home.join(".tokenmanager/control");
    std::fs::create_dir_all(&control_dir)?;
    std::fs::set_permissions(&control_dir, std::fs::Permissions::from_mode(0o700))?;
    let listener = UnixListener::bind(control_dir.join("tokenmanager.sock"))?;
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept control connection");
        let mut reader = BufReader::new(stream.try_clone().expect("clone control stream"));
        for expected_state in ["absent", "aborted"] {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read control request");
            let request: serde_json::Value = serde_json::from_str(&line).expect("control JSON");
            tx.send(
                request["method"]
                    .as_str()
                    .expect("control method")
                    .to_string(),
            )
            .expect("send observed method");
            let response = serde_json::json!({
                "ok": true,
                "state": expected_state,
                "generation": 1,
            });
            writeln!(stream, "{response}").expect("write control response");
        }
    });
    Ok((rx, handle))
}
