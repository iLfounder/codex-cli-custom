use super::*;
use crate::app::test_support::make_test_app;

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
