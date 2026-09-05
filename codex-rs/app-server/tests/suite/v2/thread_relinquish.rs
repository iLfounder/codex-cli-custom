use anyhow::Result;
use std::fs::OpenOptions;
use std::io::Write;

use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeListParams;
use codex_app_server_protocol::SessionRuntimeListResponse;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeOperationUpdatedNotification;
use codex_app_server_protocol::SessionRuntimePersistenceHealth;
use codex_app_server_protocol::SessionRuntimeWriterState;
use codex_app_server_protocol::ThreadClosedNotification;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadRelinquishParams;
use codex_app_server_protocol::ThreadRelinquishResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test]
async fn idle_relinquish_publishes_released_then_closed_and_not_loaded() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = create_mock_responses_server_repeating_assistant("unused").await;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Sqlite)
        .write(codex_home.path())?;
    let sqlite_home = codex_home.path().to_string_lossy();
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("CODEX_SQLITE_HOME", Some(sqlite_home.as_ref()))])
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.2".to_string()),
            ..Default::default()
        })
        .await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: "persist before relinquish".to_string(),
            text_elements: Vec::new(),
        }],
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
    let snapshot = before.data.into_iter().next().expect("runtime snapshot");

    let response: ThreadRelinquishResponse = app
        .request(|request_id| ClientRequest::ThreadRelinquish {
            request_id,
            params: ThreadRelinquishParams {
                operation_id: "release-idle-thread".to_string(),
                thread_id: thread.id.clone(),
                expected_instance_epoch: before.instance_epoch,
                expected_state_revision: snapshot.state_revision,
                expected_writer_generation: snapshot
                    .writer
                    .writer_generation
                    .expect("writer generation"),
            },
        })
        .await?;
    assert_eq!(
        response.operation.status,
        SessionRuntimeOperationStatus::Released
    );
    let mut released_seen = false;
    while !released_seen {
        let update: SessionRuntimeOperationUpdatedNotification = app
            .read_notification("sessionRuntime/operation/updated")
            .await?;
        released_seen = update.operation.status == SessionRuntimeOperationStatus::Released;
    }
    let closed: ThreadClosedNotification = app.read_notification("thread/closed").await?;
    assert_eq!(closed.thread_id, thread.id);

    let after: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(thread.id),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let terminal = after.data.into_iter().next().expect("terminal snapshot");
    assert_eq!(
        (terminal.lifecycle.state, terminal.writer.state),
        (
            SessionRuntimeLifecycleState::NotLoaded,
            SessionRuntimeWriterState::None,
        )
    );
    Ok(())
}

#[tokio::test]
async fn projection_failure_still_publishes_released_closed_and_degraded() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = create_mock_responses_server_repeating_assistant("unused").await;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Sqlite)
        .write(codex_home.path())?;
    let sqlite_home = codex_home.path().to_string_lossy();
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("CODEX_SQLITE_HOME", Some(sqlite_home.as_ref()))])
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.2".to_string()),
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: "persist before projection failure".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    let rollout_path = codex_utils_absolute_path::AbsolutePathBuf::try_from(
        thread.path.as_ref().expect("thread rollout path").clone(),
    )?;
    let rollout = std::fs::read_to_string(rollout_path.as_path())?;
    let duplicate = rollout.lines().last().expect("last rollout record");
    writeln!(
        OpenOptions::new()
            .append(true)
            .open(rollout_path.as_path())?,
        "{duplicate}"
    )?;

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
    let snapshot = before.data.into_iter().next().expect("runtime snapshot");
    let response: ThreadRelinquishResponse = app
        .request(|request_id| ClientRequest::ThreadRelinquish {
            request_id,
            params: ThreadRelinquishParams {
                operation_id: "release-projection-failure".to_string(),
                thread_id: thread.id.clone(),
                expected_instance_epoch: before.instance_epoch,
                expected_state_revision: snapshot.state_revision,
                expected_writer_generation: snapshot
                    .writer
                    .writer_generation
                    .expect("writer generation"),
            },
        })
        .await?;
    assert_eq!(
        response.operation.status,
        SessionRuntimeOperationStatus::Released
    );
    let mut released_seen = false;
    while !released_seen {
        let update: SessionRuntimeOperationUpdatedNotification = app
            .read_notification("sessionRuntime/operation/updated")
            .await?;
        released_seen = update.operation.status == SessionRuntimeOperationStatus::Released;
    }
    let closed: ThreadClosedNotification = app.read_notification("thread/closed").await?;
    assert_eq!(closed.thread_id, thread.id);

    let after: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(thread.id),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let terminal = after.data.into_iter().next().expect("terminal snapshot");
    assert_eq!(
        (
            terminal.lifecycle.state,
            terminal.writer.state,
            terminal.persistence.flush_health,
            terminal.persistence.materialize_health,
            terminal.persistence.deny_reason.as_deref(),
        ),
        (
            SessionRuntimeLifecycleState::NotLoaded,
            SessionRuntimeWriterState::None,
            SessionRuntimePersistenceHealth::Healthy,
            SessionRuntimePersistenceHealth::Degraded,
            Some("projection_failed"),
        )
    );
    let repeated: SessionRuntimeListResponse = app
        .request(|request_id| ClientRequest::SessionRuntimeList {
            request_id,
            params: SessionRuntimeListParams {
                thread_id: Some(terminal.thread_id),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let repeated = repeated.data.into_iter().next().expect("repeated snapshot");
    assert_eq!(
        (
            repeated.persistence.flush_health,
            repeated.persistence.materialize_health,
            repeated.persistence.deny_reason.as_deref(),
        ),
        (
            SessionRuntimePersistenceHealth::Healthy,
            SessionRuntimePersistenceHealth::Degraded,
            Some("projection_failed"),
        )
    );
    Ok(())
}
