use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeListParams;
use codex_app_server_protocol::SessionRuntimeListResponse;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_app_server_protocol::ThreadAccountSwitchParams;
use codex_app_server_protocol::ThreadAccountSwitchResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::matchers::method;
use wiremock::matchers::path;

async fn runtime_snapshot(
    app: &mut TestAppServer,
    thread_id: &str,
) -> Result<SessionRuntimeSnapshot> {
    let response: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(thread_id.to_string()),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    Ok(response.data.into_iter().next().expect("runtime snapshot"))
}

#[tokio::test]
async fn first_active_runtime_snapshot_uses_loaded_current_account_before_materialization()
-> Result<()> {
    let codex_home = TempDir::new()?;
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "done"),
        responses::ev_completed("resp-1"),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(responses::sse_response(body).set_delay(Duration::from_secs(2)))
        .expect(0..=1)
        .mount(&server)
        .await;
    MockResponsesConfig::new(&server.uri())
        .with_provider_config("requires_openai_auth = true")
        .write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("CODEX_API_KEY", Some("default-test-key"))])
        .build_initialized()
        .await?;

    let ThreadStartResponse { thread, .. } = app
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let request_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "hold the turn active".to_string(),
                text_elements: Vec::new(),
            }],
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } = app.read_response(request_id).await?;
    app.read_stream_until_notification_message("turn/started")
        .await?;

    let snapshot = runtime_snapshot(&mut app, &thread.id).await?;
    let current = snapshot
        .account
        .current
        .clone()
        .expect("loaded current account");
    assert_eq!(
        (
            snapshot.lifecycle.state,
            snapshot.lifecycle.active_turn_id,
            snapshot.account.active_turn,
        ),
        (
            SessionRuntimeLifecycleState::Active,
            Some(turn.id.clone()),
            Some(current),
        )
    );

    let _: TurnInterruptResponse = app
        .request(|request_id| ClientRequest::TurnInterrupt {
            request_id,
            params: TurnInterruptParams {
                thread_id: thread.id,
                turn_id: turn.id,
            },
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn idle_thread_switches_account_for_the_next_turn_without_reloading() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = create_mock_responses_server_repeating_assistant("switched account").await;
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
                api_key: "target-test-key".to_string(),
            })?),
        )
        .await?;
    let login: AccountSlotLoginStartResponse = app.read_response(login_request).await?;
    let target_slot_id = login.slot.account_slot_id;

    let ThreadStartResponse { thread, .. } = app
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: "materialize the initial account binding".to_string(),
            text_elements: Vec::new(),
        }],
        model: Some("mock-model".to_string()),
        ..Default::default()
    })
    .await?;
    let before: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(thread.id.clone()),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let before_snapshot = before.data.into_iter().next().expect("runtime snapshot");
    let before_account = before_snapshot
        .account
        .current
        .as_ref()
        .expect("initial account binding")
        .clone();
    let before_writer = before_snapshot.writer.clone();

    let unavailable: ThreadAccountSwitchResponse = app
        .request(|request_id| ClientRequest::ThreadAccountSwitch {
            request_id,
            params: ThreadAccountSwitchParams {
                operation_id: "switch-to-missing-slot".to_string(),
                thread_id: thread.id.clone(),
                target_account_slot_id: "missing-slot".to_string(),
                expected_instance_epoch: before.instance_epoch,
                expected_state_revision: before_snapshot.state_revision,
                expected_execution_generation: before_account.execution_generation,
            },
        })
        .await?;
    assert_eq!(
        unavailable.operation.status,
        SessionRuntimeOperationStatus::Failed
    );

    let restored: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(thread.id.clone()),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let restored_snapshot = restored.data.into_iter().next().expect("runtime snapshot");
    assert_eq!(restored_snapshot.writer, before_writer);
    assert_eq!(
        restored_snapshot.account.current,
        Some(before_account.clone())
    );

    let switched: ThreadAccountSwitchResponse = app
        .request(|request_id| ClientRequest::ThreadAccountSwitch {
            request_id,
            params: ThreadAccountSwitchParams {
                operation_id: "switch-to-private-slot".to_string(),
                thread_id: thread.id.clone(),
                target_account_slot_id: target_slot_id.clone(),
                expected_instance_epoch: restored.instance_epoch,
                expected_state_revision: restored_snapshot.state_revision,
                expected_execution_generation: before_account.execution_generation,
            },
        })
        .await?;
    assert_eq!(
        switched.operation.status,
        SessionRuntimeOperationStatus::Ready
    );

    let after: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(thread.id.clone()),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let after_snapshot = after.data.into_iter().next().expect("runtime snapshot");
    assert_eq!(after_snapshot.thread_id, thread.id);
    assert_eq!(after_snapshot.writer, before_writer);
    assert_eq!(
        after_snapshot.account.current.as_ref().map(|account| (
            account.account_slot_id.as_str(),
            account.execution_generation,
        )),
        Some((
            target_slot_id.as_str(),
            before_account.execution_generation + 1,
        ))
    );
    assert_eq!(after_snapshot.lifecycle.subscriber_count, 1);

    let turn_request = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![UserInput::Text {
                text: "use the switched account".to_string(),
                text_elements: Vec::new(),
            }],
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app.read_response(turn_request).await?;
    app.read_stream_until_notification_message("turn/completed")
        .await?;

    let requests = server.received_requests().await.expect("received requests");
    let authorization = requests
        .last()
        .and_then(|request| request.headers.get("authorization"))
        .and_then(|value| value.to_str().ok());
    assert_eq!(authorization, Some("Bearer target-test-key"));

    let switched_snapshot: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(after_snapshot.thread_id.clone()),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let switched_snapshot = switched_snapshot
        .data
        .into_iter()
        .next()
        .expect("switched runtime snapshot");
    let switched_account = switched_snapshot
        .account
        .current
        .as_ref()
        .expect("switched account binding")
        .clone();
    let state_db = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    let forced = state_db
        .compare_and_swap_execution_account_binding(
            codex_protocol::ThreadId::from_string(&after_snapshot.thread_id)?,
            &codex_protocol::protocol::ExecutionAccountBinding {
                slot_id: switched_account.account_slot_id.clone(),
                generation: switched_account.execution_generation,
            },
            "forced-stale",
        )
        .await?;
    assert!(forced.is_some(), "test must force the durable CAS to fail");

    let failed: ThreadAccountSwitchResponse = app
        .request(|request_id| ClientRequest::ThreadAccountSwitch {
            request_id,
            params: ThreadAccountSwitchParams {
                operation_id: "switch-with-stale-durable-binding".to_string(),
                thread_id: switched_snapshot.thread_id.clone(),
                target_account_slot_id: before_account.account_slot_id.clone(),
                expected_instance_epoch: after.instance_epoch,
                expected_state_revision: switched_snapshot.state_revision,
                expected_execution_generation: switched_account.execution_generation,
            },
        })
        .await?;
    assert_eq!(
        failed.operation.status,
        SessionRuntimeOperationStatus::Failed
    );

    let after_failed_cas: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(switched_snapshot.thread_id.clone()),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let after_failed_cas = after_failed_cas
        .data
        .into_iter()
        .next()
        .expect("runtime snapshot after failed CAS");
    assert_eq!(after_failed_cas.account.current, Some(switched_account));

    let turn_request = app
        .send_turn_start_request(TurnStartParams {
            thread_id: switched_snapshot.thread_id,
            input: vec![UserInput::Text {
                text: "keep using the account after the failed switch".to_string(),
                text_elements: Vec::new(),
            }],
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app.read_response(turn_request).await?;
    app.read_stream_until_notification_message("turn/completed")
        .await?;
    let requests = server.received_requests().await.expect("received requests");
    let authorization = requests
        .last()
        .and_then(|request| request.headers.get("authorization"))
        .and_then(|value| value.to_str().ok());
    assert_eq!(authorization, Some("Bearer target-test-key"));
    Ok(())
}
