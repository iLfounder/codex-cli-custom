use super::*;
use crate::app::test_support::make_test_app;
use crate::app::test_support::make_test_app_with_channels;
use crate::app_event::AppEvent;
use codex_app_server_protocol::SessionRuntimeLifecycle;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeOperationError;
use codex_app_server_protocol::SessionRuntimeWriterState;

fn released_operation(thread_id: ThreadId) -> SessionRuntimeOperation {
    SessionRuntimeOperation {
        operation_id: "release-op".to_string(),
        request_fingerprint: "fingerprint".to_string(),
        action: SessionRuntimeOperationAction::ThreadRelinquish,
        status: SessionRuntimeOperationStatus::Released,
        thread_id: Some(thread_id.to_string()),
        account_slot_id: None,
        state_revision: Some(2),
        writer_generation: Some(3),
        execution_generation: None,
        error: None,
        updated_at: 0,
    }
}

fn pending(thread_id: ThreadId) -> PendingShutdown {
    PendingShutdown {
        intent: ShutdownIntent::Exit,
        thread_id,
        operation_id: "release-op".to_string(),
        instance_epoch: "epoch".to_string(),
        state_revision: 1,
        writer_generation: 3,
        released: false,
        thread_closed: false,
    }
}

fn failed_operation(thread_id: ThreadId, code: &str, message: &str) -> SessionRuntimeOperation {
    SessionRuntimeOperation {
        status: SessionRuntimeOperationStatus::Failed,
        error: Some(SessionRuntimeOperationError {
            code: code.to_string(),
            message: message.to_string(),
        }),
        ..released_operation(thread_id)
    }
}

#[tokio::test]
async fn shutdown_finishes_only_after_released_then_thread_closed() {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    app.pending_shutdown = Some(pending(thread_id));

    assert!(app.handle_shutdown_operation(Some("epoch"), &released_operation(thread_id)));
    assert!(app.pending_shutdown.is_some());
    assert!(app.handle_pending_shutdown_thread_closed(&thread_id.to_string()));
    assert!(app.pending_shutdown.is_none());
}

#[tokio::test]
async fn shutdown_finishes_only_after_thread_closed_then_released() {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    app.pending_shutdown = Some(pending(thread_id));

    assert!(app.handle_pending_shutdown_thread_closed(&thread_id.to_string()));
    assert!(app.pending_shutdown.is_some());
    assert!(app.handle_shutdown_operation(Some("epoch"), &released_operation(thread_id)));
    assert!(app.pending_shutdown.is_none());
}

#[tokio::test]
async fn thread_not_idle_failure_keeps_safe_release_retryable() {
    let (mut app, mut events, _op_rx) = make_test_app_with_channels().await;
    let thread_id = ThreadId::new();
    app.pending_shutdown = Some(pending(thread_id));

    assert!(app.handle_shutdown_operation(
        Some("epoch"),
        &failed_operation(thread_id, "thread_not_idle", "thread became busy"),
    ));
    assert!(app.pending_shutdown.is_none());
    assert!(!app.shutdown_force_exit_armed);

    let rendered = loop {
        let event = events.try_recv().expect("expected shutdown error event");
        if let AppEvent::InsertHistoryCell(cell) = event {
            break cell
                .display_lines(/*width*/ 80)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
    };
    insta::assert_snapshot!(rendered);
}

#[tokio::test]
async fn authoritative_release_failure_preserves_immediate_exit_escape() {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    app.pending_shutdown = Some(pending(thread_id));

    assert!(app.handle_shutdown_operation(
        Some("epoch"),
        &failed_operation(thread_id, "durability_failed", "writer flush failed"),
    ));
    assert!(app.pending_shutdown.is_none());
    assert!(app.shutdown_force_exit_armed);
}

#[test]
fn non_idle_foreign_writer_preserves_immediate_exit_escape() {
    let lifecycle = SessionRuntimeLifecycle {
        state: SessionRuntimeLifecycleState::Active,
        active_turn_id: Some("turn-1".to_string()),
        waiting_on: Vec::new(),
        subscriber_count: 1,
        client_incarnations: Vec::new(),
        last_activity_at: None,
        unload_at: None,
    };

    assert_eq!(
        shutdown_failure_disposition(SessionRuntimeWriterState::OwnedElsewhere, &lifecycle),
        ShutdownFailureDisposition::AllowImmediateExit
    );
    assert_eq!(
        shutdown_failure_disposition(SessionRuntimeWriterState::OwnedHere, &lifecycle),
        ShutdownFailureDisposition::RetrySafeRelease
    );
}
