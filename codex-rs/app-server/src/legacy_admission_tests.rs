use super::*;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RequestId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnCompleteEvent;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::timeout;

const INSTANCE_GENERATION: &str = "11111111-1111-4111-8111-111111111111";
const CUTOVER_EPOCH: &str = "cutover-epoch-1";

#[tokio::test]
async fn seal_waits_for_admitted_mutation_then_blocks_new_mutations_until_abort() {
    let gate = LegacyAdmissionGate::enabled(INSTANCE_GENERATION.to_string());
    let permit = gate
        .admit(&request("thread/start", json!({})))
        .expect("open gate should admit a mutation");

    let sealing_gate = gate.clone();
    let seal_task = tokio::spawn(async move { sealing_gate.seal(seal_params()).await });
    let sealing = wait_for_state(&gate, LegacyAdmissionState::Sealing).await;
    assert_eq!(
        sealing,
        LegacyAdmissionSnapshot {
            cutover_epoch: CUTOVER_EPOCH.to_string(),
            app_server_instance_generation: INSTANCE_GENERATION.to_string(),
            state: LegacyAdmissionState::Sealing,
            in_flight_mutation_count: 1,
        }
    );

    drop(permit);
    let sealed = timeout(Duration::from_secs(1), seal_task)
        .await
        .expect("seal should finish after the permit is released")
        .expect("seal task should not panic")
        .expect("seal should succeed");
    assert_eq!(sealed.admission.state, LegacyAdmissionState::Drained);

    let rejection = match gate.admit(&request("turn/start", turn_start_params())) {
        Ok(_) => panic!("sealed gate should reject a new root turn"),
        Err(error) => error,
    };
    assert!(rejection.message.contains("is sealed"));
    let guardian_approval = match gate.admit(&request(
        "thread/approveGuardianDeniedAction",
        json!({"threadId": "thread-1", "event": {"type": "guardian"}}),
    )) {
        Ok(_) => panic!("guardian-denied approval must not start executable work after seal"),
        Err(error) => error,
    };
    assert!(guardian_approval.message.contains("is sealed"));
    gate.admit(&request("thread/list", json!({})))
        .expect("reads should remain available while sealed");
    gate.admit(&request("turn/interrupt", turn_interrupt_params()))
        .expect("cancellation should remain available while sealed");

    let first_abort = gate.abort(abort_params()).expect("abort should succeed");
    let second_abort = gate
        .abort(abort_params())
        .expect("same abort should be idempotent");
    assert_eq!(first_abort, second_abort);
    assert_eq!(first_abort.admission.state, LegacyAdmissionState::Aborted);
    gate.admit(&request("thread/start", json!({})))
        .expect("abort should reopen mutation admission");
}

#[tokio::test]
async fn admitted_root_turn_holds_drain_until_terminal_event() {
    let gate = LegacyAdmissionGate::enabled(INSTANCE_GENERATION.to_string());
    let permit = gate
        .admit(&request("turn/start", turn_start_params()))
        .expect("open gate should admit root turn");
    let mut thread_state = crate::thread_state::ThreadState::default();
    thread_state.register_admitted_turn("turn-1".to_string(), Some(permit));

    let sealing_gate = gate.clone();
    let seal_task = tokio::spawn(async move { sealing_gate.seal(seal_params()).await });
    let sealing = wait_for_state(&gate, LegacyAdmissionState::Sealing).await;
    assert_eq!(sealing.in_flight_mutation_count, 1);

    thread_state.track_current_turn_event(
        "turn-1",
        &EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    );

    let sealed = timeout(Duration::from_secs(1), seal_task)
        .await
        .expect("seal should finish after terminal event")
        .expect("seal task should not panic")
        .expect("seal should succeed");
    assert_eq!(sealed.admission.state, LegacyAdmissionState::Drained);
}

#[tokio::test]
async fn sealed_gate_drops_executable_client_responses_until_abort() {
    let gate = LegacyAdmissionGate::enabled(INSTANCE_GENERATION.to_string());
    assert!(gate.accepts_client_response());

    gate.seal(seal_params()).await.expect("seal should succeed");
    assert!(!gate.accepts_client_response());

    gate.abort(abort_params())
        .expect("abort should reopen gate");
    assert!(gate.accepts_client_response());
}

#[tokio::test]
async fn seal_is_idempotent_and_rejects_stale_epoch_or_instance() {
    let gate = LegacyAdmissionGate::enabled(INSTANCE_GENERATION.to_string());
    let first = gate.seal(seal_params()).await.expect("seal should succeed");
    let second = gate
        .seal(seal_params())
        .await
        .expect("same seal should be idempotent");
    assert_eq!(first, second);

    let stale_epoch = gate
        .status(LegacyAdmissionStatusParams {
            cutover_epoch: "other-epoch".to_string(),
            expected_app_server_instance_generation: INSTANCE_GENERATION.to_string(),
        })
        .expect_err("different epoch should fail closed");
    assert!(stale_epoch.message.contains("epoch mismatch"));

    let stale_instance = gate
        .status(LegacyAdmissionStatusParams {
            cutover_epoch: CUTOVER_EPOCH.to_string(),
            expected_app_server_instance_generation: "replacement-instance".to_string(),
        })
        .expect_err("different process identity should fail closed");
    assert!(stale_instance.message.contains("generation mismatch"));
}

#[test]
fn disabled_gate_rejects_transition_control_but_does_not_change_normal_admission() {
    let gate = LegacyAdmissionGate::default();
    let error = gate
        .status(status_params())
        .expect_err("ordinary app-server should not expose legacy control");
    assert_eq!(error.message, UNSUPPORTED_MESSAGE);
    gate.admit(&request("thread/start", json!({})))
        .expect("ordinary app-server admission should be unchanged");
}

async fn wait_for_state(
    gate: &LegacyAdmissionGate,
    expected: LegacyAdmissionState,
) -> LegacyAdmissionSnapshot {
    timeout(Duration::from_secs(1), async {
        loop {
            match gate.status(status_params()) {
                Ok(response) if response.admission.state == expected => return response.admission,
                Ok(_) | Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("state should become visible")
}

fn request(method: &str, params: serde_json::Value) -> ClientRequest {
    ClientRequest::try_from(JSONRPCRequest {
        id: RequestId::Integer(1),
        method: method.to_string(),
        params: Some(params),
        trace: None,
    })
    .expect("test request should deserialize")
}

fn seal_params() -> LegacyAdmissionSealParams {
    LegacyAdmissionSealParams {
        cutover_epoch: CUTOVER_EPOCH.to_string(),
        expected_app_server_instance_generation: INSTANCE_GENERATION.to_string(),
    }
}

fn status_params() -> LegacyAdmissionStatusParams {
    LegacyAdmissionStatusParams {
        cutover_epoch: CUTOVER_EPOCH.to_string(),
        expected_app_server_instance_generation: INSTANCE_GENERATION.to_string(),
    }
}

fn abort_params() -> LegacyAdmissionAbortParams {
    LegacyAdmissionAbortParams {
        cutover_epoch: CUTOVER_EPOCH.to_string(),
        expected_app_server_instance_generation: INSTANCE_GENERATION.to_string(),
    }
}

fn turn_start_params() -> serde_json::Value {
    json!({
        "threadId": "11111111-1111-4111-8111-111111111111",
        "input": [{"type": "text", "text": "hello", "textElements": []}]
    })
}

fn turn_interrupt_params() -> serde_json::Value {
    json!({
        "threadId": "11111111-1111-4111-8111-111111111111",
        "turnId": "22222222-2222-4222-8222-222222222222"
    })
}
