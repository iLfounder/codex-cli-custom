use std::time::Duration;

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotActionAvailability;
use codex_app_server_protocol::AccountSlotChangedNotification;
use codex_app_server_protocol::AccountSlotListParams;
use codex_app_server_protocol::AccountSlotListResponse;
use codex_app_server_protocol::AccountSlotLoginChallenge;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::CancelLoginAccountParams;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SessionRuntimeChangedNotification;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeListParams;
use codex_app_server_protocol::SessionRuntimeListResponse;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeOperationUpdatedNotification;
use codex_app_server_protocol::ThreadAccountSwitchParams;
use codex_app_server_protocol::ThreadAccountSwitchResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_config::types::AuthCredentialsStoreMode;
use codex_features::Feature;
use codex_login::AuthKeyringBackendKind;
use codex_login::login_with_api_key;
use codex_login::logout;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serial_test::serial;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_ISSUER_ENV_VAR: &str = "CODEX_APP_SERVER_LOGIN_ISSUER";

#[tokio::test]
async fn default_logout_projection_and_rpc_share_reloaded_provider_policy() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"
cli_auth_credentials_store = "file"

[features]
shell_snapshot = false
"#,
    )?;
    login_with_api_key(
        codex_home.path(),
        "default-test-secret",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
model_provider = "amazon-bedrock"
cli_auth_credentials_store = "file"

[model_providers.amazon-bedrock.aws]
profile = "fixture"
region = "us-west-2"
"#,
    )?;
    let request_id = app_server
        .send_raw_request(
            "accountSlot/list",
            Some(serde_json::to_value(AccountSlotListParams {
                cursor: None,
                limit: None,
            })?),
        )
        .await?;
    let listed: AccountSlotListResponse =
        timeout(DEFAULT_TIMEOUT, app_server.read_response(request_id)).await??;
    let logout = listed
        .data
        .iter()
        .find(|slot| slot.is_default)
        .and_then(|slot| {
            slot.actions
                .iter()
                .find(|action| action.action == AccountSlotAction::Logout)
        })
        .expect("default logout action");
    assert_eq!(
        logout,
        &AccountSlotActionAvailability {
            action: AccountSlotAction::Logout,
            allowed: false,
            deny_reason: Some("provider_managed_logout_not_allowed".to_string()),
        }
    );

    let request_id = app_server.send_logout_account_request().await?;
    let error = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, -32600);
    assert!(codex_home.path().join("auth.json").is_file());
    Ok(())
}

#[tokio::test]
#[serial(login_port)]
async fn secondary_cancel_reports_projection_persistence_failure() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
model = "mock-model"
cli_auth_credentials_store = "file"
"#,
    )?;
    login_with_api_key(
        codex_home.path(),
        "default-test-secret",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            (LOGIN_ISSUER_ENV_VAR, Some("https://auth.example.com")),
        ])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = app_server
        .send_raw_request(
            "accountSlot/login/start",
            Some(serde_json::to_value(
                AccountSlotLoginStartParams::Chatgpt {
                    slot_id: None,
                    codex_streamlined_login: false,
                    use_hosted_login_success_page: false,
                    app_brand: None,
                },
            )?),
        )
        .await?;
    let started: AccountSlotLoginStartResponse =
        timeout(DEFAULT_TIMEOUT, app_server.read_response(request_id)).await??;
    let Some(AccountSlotLoginChallenge::Browser { login_id, .. }) = started.challenge else {
        anyhow::bail!("expected browser login challenge");
    };

    let manifest = codex_home.path().join("account-slots.toml");
    std::fs::rename(
        &manifest,
        codex_home.path().join("account-slots.before-cancel.toml"),
    )?;
    std::fs::create_dir(&manifest)?;
    let request_id = app_server
        .send_cancel_login_account_request(CancelLoginAccountParams { login_id })
        .await?;
    let error = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, -32603);
    Ok(())
}

#[tokio::test]
async fn api_key_login_creates_ready_private_slot_and_sanitized_events() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"
cli_auth_credentials_store = "file"

[features]
shell_snapshot = false
"#,
    )?;
    login_with_api_key(
        codex_home.path(),
        "default-test-secret",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("CODEX_API_KEY", None),
            ("CODEX_ACCESS_TOKEN", None),
            ("OPENAI_API_KEY", None),
        ])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = app_server
        .send_raw_request(
            "accountSlot/login/start",
            Some(serde_json::to_value(AccountSlotLoginStartParams::ApiKey {
                slot_id: None,
                api_key: "test-slot-secret".to_string(),
            })?),
        )
        .await?;
    let response: AccountSlotLoginStartResponse =
        timeout(DEFAULT_TIMEOUT, app_server.read_response(request_id)).await??;
    let accepted: SessionRuntimeOperationUpdatedNotification = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_notification("sessionRuntime/operation/updated"),
    )
    .await??;
    let started: AccountSlotChangedNotification = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_notification("accountSlot/changed"),
    )
    .await??;
    let changed: AccountSlotChangedNotification = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_notification("accountSlot/changed"),
    )
    .await??;
    let ready: SessionRuntimeOperationUpdatedNotification = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_notification("sessionRuntime/operation/updated"),
    )
    .await??;

    assert_eq!(response.slot.status, AccountSlotStatus::Ready);
    assert_eq!(response.challenge, None);
    assert_eq!(
        (
            started.slot.status,
            started.slot.active_login_operation_id.as_deref(),
            started.slot.attempt_generation,
        ),
        (
            AccountSlotStatus::LoginRequired,
            None,
            response.slot.attempt_generation,
        )
    );
    assert_eq!(changed.slot, response.slot);
    assert_eq!(
        accepted.operation.status,
        SessionRuntimeOperationStatus::Accepted
    );
    assert_eq!(ready.operation, response.operation);
    let response_json = serde_json::to_string(&response)?;
    assert!(!response_json.contains("test-slot-secret"));
    assert!(!response_json.contains(codex_home.path().to_string_lossy().as_ref()));

    let request_id = app_server
        .send_raw_request(
            "accountSlot/list",
            Some(serde_json::to_value(AccountSlotListParams {
                cursor: None,
                limit: None,
            })?),
        )
        .await?;
    let listed: AccountSlotListResponse =
        timeout(DEFAULT_TIMEOUT, app_server.read_response(request_id)).await??;
    assert_eq!(
        listed.registry_revision,
        response.slot.registry_revision.saturating_add(1)
    );
    let mut switch_actions = listed
        .data
        .iter()
        .map(|slot| {
            (
                slot.is_default,
                slot.status,
                slot.actions
                    .iter()
                    .find(|action| action.action == AccountSlotAction::SwitchTo)
                    .cloned(),
            )
        })
        .collect::<Vec<_>>();
    switch_actions.sort_by_key(|(is_default, _, _)| *is_default);
    assert_eq!(
        switch_actions,
        vec![
            (
                false,
                AccountSlotStatus::Ready,
                Some(AccountSlotActionAvailability {
                    action: AccountSlotAction::SwitchTo,
                    allowed: true,
                    deny_reason: None,
                }),
            ),
            (
                true,
                AccountSlotStatus::Ready,
                Some(AccountSlotActionAvailability {
                    action: AccountSlotAction::SwitchTo,
                    allowed: true,
                    deny_reason: None,
                }),
            ),
        ]
    );

    let secondary_home = codex_home
        .path()
        .join("accounts")
        .join(&response.slot.account_slot_id)
        .join(format!(
            "runtime-{}",
            response
                .operation
                .execution_generation
                .expect("login operation execution generation")
        ));
    assert!(logout(
        &secondary_home,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?);
    let request_id = app_server
        .send_raw_request(
            "accountSlot/list",
            Some(serde_json::to_value(AccountSlotListParams {
                cursor: None,
                limit: None,
            })?),
        )
        .await?;
    let missing: AccountSlotListResponse =
        timeout(DEFAULT_TIMEOUT, app_server.read_response(request_id)).await??;
    let missing_secondary = missing
        .data
        .iter()
        .find(|slot| slot.account_slot_id == response.slot.account_slot_id)
        .expect("secondary slot after auth removal");
    assert_eq!(
        (
            missing_secondary.status,
            missing_secondary.error_code.as_deref(),
            missing_secondary.registry_revision,
        ),
        (
            AccountSlotStatus::Failed,
            Some("authUnavailable"),
            missing.registry_revision,
        )
    );
    assert!(missing.registry_revision > listed.registry_revision);

    let request_id = app_server
        .send_raw_request(
            "accountSlot/list",
            Some(serde_json::to_value(AccountSlotListParams {
                cursor: None,
                limit: None,
            })?),
        )
        .await?;
    let unchanged: AccountSlotListResponse =
        timeout(DEFAULT_TIMEOUT, app_server.read_response(request_id)).await??;
    assert_eq!(unchanged, missing);
    Ok(())
}

#[tokio::test]
async fn ready_secondary_slot_reauth_rebinds_loaded_idle_thread_for_next_turn() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = create_mock_responses_server_repeating_assistant("reauthenticated account").await;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Sqlite)
        .with_provider_config("requires_openai_auth = true")
        .write(codex_home.path())?;
    let sqlite_home = codex_home.path().to_string_lossy();
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[
            ("CODEX_API_KEY", Some("default-test-key")),
            ("CODEX_ACCESS_TOKEN", None),
            ("OPENAI_API_KEY", None),
            ("CODEX_SQLITE_HOME", Some(sqlite_home.as_ref())),
        ])
        .build_initialized()
        .await?;

    let login_request = app
        .send_raw_request(
            "accountSlot/login/start",
            Some(serde_json::to_value(AccountSlotLoginStartParams::ApiKey {
                slot_id: None,
                api_key: "original-secondary-key".to_string(),
            })?),
        )
        .await?;
    let login: AccountSlotLoginStartResponse = app.read_response(login_request).await?;
    let secondary_slot_id = login.slot.account_slot_id;

    let ThreadStartResponse { thread, .. } = app
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: "materialize the default account binding".to_string(),
            text_elements: Vec::new(),
        }],
        model: Some("mock-model".to_string()),
        ..Default::default()
    })
    .await?;
    let before_switch: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(thread.id.clone()),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let before_switch_snapshot = before_switch
        .data
        .into_iter()
        .next()
        .expect("runtime snapshot before account switch");
    let default_binding = before_switch_snapshot
        .account
        .current
        .as_ref()
        .expect("default account binding");
    let switched: ThreadAccountSwitchResponse = app
        .request(|request_id| ClientRequest::ThreadAccountSwitch {
            request_id,
            params: ThreadAccountSwitchParams {
                operation_id: "bind-thread-to-secondary-before-reauth".to_string(),
                thread_id: thread.id.clone(),
                target_account_slot_id: secondary_slot_id.clone(),
                expected_instance_epoch: before_switch.instance_epoch,
                expected_state_revision: before_switch_snapshot.state_revision,
                expected_execution_generation: default_binding.execution_generation,
            },
        })
        .await?;
    assert_eq!(
        switched.operation.status,
        SessionRuntimeOperationStatus::Ready
    );

    let before_reauth: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(thread.id.clone()),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let before_reauth_snapshot = before_reauth
        .data
        .into_iter()
        .next()
        .expect("runtime snapshot before reauthentication");
    let before_reauth_binding = before_reauth_snapshot
        .account
        .current
        .clone()
        .expect("secondary account binding before reauthentication");
    assert_eq!(
        before_reauth_binding.account_slot_id.as_str(),
        secondary_slot_id
    );
    assert_eq!(
        before_reauth_snapshot.lifecycle.state,
        SessionRuntimeLifecycleState::Idle
    );

    let thread_id = codex_protocol::ThreadId::from_string(&thread.id)?;
    let state_db = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    let durable_before = state_db
        .execution_account_slot_runtime_state(&secondary_slot_id)
        .await?;
    assert_eq!(
        durable_before.1,
        vec![(
            thread_id,
            codex_protocol::protocol::ExecutionAccountBinding {
                slot_id: secondary_slot_id.clone(),
                generation: before_reauth_binding.execution_generation,
            },
        )]
    );

    let reauth_request = app
        .send_raw_request(
            "accountSlot/login/start",
            Some(serde_json::to_value(AccountSlotLoginStartParams::ApiKey {
                slot_id: Some(secondary_slot_id.clone()),
                api_key: "replacement-secondary-key".to_string(),
            })?),
        )
        .await?;
    let reauth: AccountSlotLoginStartResponse = app.read_response(reauth_request).await?;
    assert_eq!(
        (
            reauth.slot.account_slot_id.as_str(),
            reauth.slot.status,
            reauth.operation.status,
        ),
        (
            secondary_slot_id.as_str(),
            AccountSlotStatus::Ready,
            SessionRuntimeOperationStatus::Ready,
        )
    );

    let expected_generation = before_reauth_binding.execution_generation + 1;
    let published = loop {
        let changed: SessionRuntimeChangedNotification = timeout(
            DEFAULT_TIMEOUT,
            app.read_notification("sessionRuntime/changed"),
        )
        .await??;
        if changed.snapshot.thread_id == thread.id
            && changed
                .snapshot
                .account
                .current
                .as_ref()
                .is_some_and(|binding| {
                    binding.account_slot_id == secondary_slot_id
                        && binding.execution_generation == expected_generation
                })
        {
            break changed;
        }
    };
    assert_eq!(
        published.snapshot.lifecycle.state,
        SessionRuntimeLifecycleState::Idle
    );

    let after_reauth: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(thread.id.clone()),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let after_reauth_snapshot = after_reauth
        .data
        .into_iter()
        .next()
        .expect("runtime snapshot after reauthentication");
    assert_eq!(
        after_reauth_snapshot.account.current,
        published.snapshot.account.current
    );
    assert_eq!(after_reauth_snapshot.writer, before_reauth_snapshot.writer);

    let durable_after = state_db
        .execution_account_slot_runtime_state(&secondary_slot_id)
        .await?;
    assert_eq!(durable_after.0, durable_before.0 + 1);
    assert_eq!(
        durable_after.1,
        vec![(
            thread_id,
            codex_protocol::protocol::ExecutionAccountBinding {
                slot_id: secondary_slot_id,
                generation: expected_generation,
            },
        )]
    );

    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id,
        input: vec![UserInput::Text {
            text: "use the replacement credential".to_string(),
            text_elements: Vec::new(),
        }],
        model: Some("mock-model".to_string()),
        ..Default::default()
    })
    .await?;
    let requests = server.received_requests().await.expect("received requests");
    let authorization = requests
        .last()
        .and_then(|request| request.headers.get("authorization"))
        .and_then(|value| value.to_str().ok());
    assert_eq!(authorization, Some("Bearer replacement-secondary-key"));
    Ok(())
}
