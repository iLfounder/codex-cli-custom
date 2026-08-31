use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ReviewDelivery;
use codex_app_server_protocol::ReviewStartParams;
use codex_app_server_protocol::ReviewStartResponse;
use codex_app_server_protocol::ReviewTarget;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;

#[tokio::test]
async fn immediate_interrupt_accepts_only_the_started_turn_id() -> Result<()> {
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "done"),
        responses::ev_completed("resp-1"),
    ]);
    let _response_mock = responses::mount_response_once(
        &server,
        responses::sse_response(body).set_delay(Duration::from_secs(2)),
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let ThreadStartResponse { thread, .. } = app
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let start_request_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: Vec::new(),
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } =
        timeout(DEFAULT_READ_TIMEOUT, app.read_response(start_request_id)).await??;

    let wrong_interrupt_id = app
        .send_turn_interrupt_request(TurnInterruptParams {
            thread_id: thread.id.clone(),
            turn_id: "wrong-turn-id".to_string(),
        })
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(wrong_interrupt_id)),
    )
    .await??;
    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);

    app.interrupt_turn_and_wait_for_aborted(thread.id, turn.id, DEFAULT_READ_TIMEOUT)
        .await?;
    Ok(())
}

#[tokio::test]
async fn immediate_interrupt_accepts_inline_review_turn_id() -> Result<()> {
    immediate_review_interrupt(ReviewDelivery::Inline).await
}

#[cfg_attr(target_os = "windows", ignore = "flaky on windows CI")]
#[tokio::test]
async fn immediate_interrupt_accepts_detached_review_turn_id() -> Result<()> {
    immediate_review_interrupt(ReviewDelivery::Detached).await
}

async fn immediate_review_interrupt(delivery: ReviewDelivery) -> Result<()> {
    let server = responses::start_mock_server().await;
    if delivery == ReviewDelivery::Detached {
        let materialize_body = responses::sse(vec![
            responses::ev_response_created("materialize-response"),
            responses::ev_assistant_message("materialize-message", "materialized"),
            responses::ev_completed("materialize-response"),
        ]);
        let _materialize_mock = responses::mount_sse_once(&server, materialize_body).await;
    }
    let review_body = responses::sse(vec![
        responses::ev_response_created("review-response"),
        responses::ev_assistant_message("review-message", "done"),
        responses::ev_completed("review-response"),
    ]);
    let _review_mock = responses::mount_response_once(
        &server,
        responses::sse_response(review_body).set_delay(Duration::from_secs(2)),
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let ThreadStartResponse { thread, .. } = app
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    if delivery == ReviewDelivery::Detached {
        let _: TurnStartResponse = app
            .request(
                |request_id| codex_app_server_protocol::ClientRequest::TurnStart {
                    request_id,
                    params: TurnStartParams {
                        thread_id: thread.id.clone(),
                        input: vec![UserInput::Text {
                            text: "materialize rollout".to_string(),
                            text_elements: Vec::new(),
                        }],
                        ..Default::default()
                    },
                },
            )
            .await?;
        timeout(
            DEFAULT_READ_TIMEOUT,
            app.read_stream_until_notification_message("turn/completed"),
        )
        .await??;
    }

    let request_id = app
        .send_review_start_request(ReviewStartParams {
            thread_id: thread.id,
            delivery: Some(delivery),
            target: ReviewTarget::Custom {
                instructions: "review this change".to_string(),
            },
        })
        .await?;
    let ReviewStartResponse {
        turn,
        review_thread_id,
    } = timeout(DEFAULT_READ_TIMEOUT, app.read_response(request_id)).await??;

    app.interrupt_turn_and_wait_for_aborted(review_thread_id, turn.id, DEFAULT_READ_TIMEOUT)
        .await?;
    Ok(())
}
