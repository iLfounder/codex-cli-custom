use super::*;
use codex_backend_client::ApiKeyResponseCost;
use codex_core::config::ConfigBuilder;
use codex_otel::TelemetryAuthMode;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const TURN_COST_PATH: &str = "/v1/analytics/codex/turn-costs";

#[tokio::test]
async fn due_turns_are_polled_by_exact_auth_manager() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TURN_COST_PATH))
        .and(header("authorization", "Bearer sk-one"))
        .and(body_json(serde_json::json!({ "turn_ids": ["turn-one"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "turns": [{
                "turn_id": "turn-one",
                "status": "priced",
                "total_usd": "1.00",
                "event_count": 0
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(TURN_COST_PATH))
        .and(header("authorization", "Bearer sk-two"))
        .and(body_json(serde_json::json!({ "turn_ids": ["turn-two"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "turns": [{
                "turn_id": "turn-two",
                "status": "priced",
                "total_usd": "2.00",
                "event_count": 0
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let auth_one = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("sk-one"));
    let auth_two = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("sk-two"));
    let mut runtime = test_runtime(&server).await;
    for (turn_id, auth_manager) in [("turn-one", auth_one), ("turn-two", auth_two)] {
        let thread_id = ThreadId::new();
        runtime.turns.insert(
            turn_id.to_string(),
            TurnCostEntry {
                thread_id,
                auth_manager,
                session_telemetry: test_session_telemetry(thread_id),
                expected_response_count: 0,
                status: TurnCostStatus::Completed,
                next_poll_at: Instant::now(),
                attempt_count: 0,
            },
        );
    }

    runtime.poll_due().await;

    assert!(runtime.turns.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn priced_cost_uses_telemetry_captured_before_thread_removal() {
    let server = MockServer::start().await;
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("sk-test"));
    let mut runtime = test_runtime(&server).await;
    let thread_id = ThreadId::new();
    let turn_id = "turn-1";

    runtime.record_observation(TurnCostObservation {
        thread_id,
        turn_id: turn_id.to_string(),
        auth_manager: Arc::clone(&auth_manager),
        kind: TurnCostObservationKind::Started {
            session_telemetry: Box::new(test_session_telemetry(thread_id)),
        },
    });
    runtime.record_observation(TurnCostObservation {
        thread_id,
        turn_id: turn_id.to_string(),
        auth_manager: Arc::clone(&auth_manager),
        kind: TurnCostObservationKind::ResponseCompleted,
    });
    runtime.record_observation(TurnCostObservation {
        thread_id,
        turn_id: turn_id.to_string(),
        auth_manager,
        kind: TurnCostObservationKind::Finished { interrupted: false },
    });

    runtime.process_api_key_cost(
        turn_id,
        &ApiKeyTurnCost {
            turn_id: turn_id.to_string(),
            status: ApiKeyTurnCostStatus::Priced,
            total_usd: Some("1.25".to_string()),
            event_count: Some(1),
            responses: None,
            model: Some("gpt-5.6".to_string()),
            speed: Some("fast".to_string()),
            reasoning_effort: Some("high".to_string()),
        },
    );

    assert_eq!(runtime.turns.len(), 0);
}

#[tokio::test]
async fn priced_cost_waits_for_every_response_when_response_costs_are_available() {
    let server = MockServer::start().await;
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("sk-test"));
    let mut runtime = test_runtime(&server).await;
    let thread_id = ThreadId::new();
    let turn_id = "turn-1";

    runtime.turns.insert(
        turn_id.to_string(),
        TurnCostEntry {
            thread_id,
            auth_manager,
            session_telemetry: test_session_telemetry(thread_id),
            expected_response_count: 2,
            status: TurnCostStatus::Completed,
            next_poll_at: Instant::now(),
            attempt_count: 0,
        },
    );

    let mut cost = ApiKeyTurnCost {
        turn_id: turn_id.to_string(),
        status: ApiKeyTurnCostStatus::Priced,
        total_usd: Some("1.25".to_string()),
        event_count: Some(2),
        responses: Some(vec![ApiKeyResponseCost {
            response_id: "resp-one".to_string(),
            total_usd: "1.25".to_string(),
        }]),
        model: Some("gpt-5.6".to_string()),
        speed: Some("fast".to_string()),
        reasoning_effort: Some("high".to_string()),
    };
    runtime.process_api_key_cost(turn_id, &cost);

    let entry = runtime.turns.get(turn_id).expect("turn remains tracked");
    assert_eq!(entry.attempt_count, 1);

    cost.event_count = None;
    cost.responses
        .as_mut()
        .expect("response costs")
        .push(ApiKeyResponseCost {
            response_id: "resp-two".to_string(),
            total_usd: "0.50".to_string(),
        });
    runtime.process_api_key_cost(turn_id, &cost);

    assert_eq!(runtime.turns.len(), 0);
}

async fn test_runtime(server: &MockServer) -> WorkerRuntime {
    let codex_home = TempDir::new().expect("temporary Codex home");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("test config");
    config.chatgpt_base_url = server.uri();
    WorkerRuntime {
        config: Arc::new(config),
        turns: HashMap::new(),
    }
}

fn test_session_telemetry(thread_id: ThreadId) -> SessionTelemetry {
    SessionTelemetry::new(
        thread_id,
        "gpt-5.6",
        "gpt-5.6",
        /*account_id*/ None,
        /*account_email*/ None,
        Some(TelemetryAuthMode::ApiKey),
        "test".to_string(),
        /*log_user_prompts*/ false,
        "test".to_string(),
        SessionSource::Cli,
    )
}
