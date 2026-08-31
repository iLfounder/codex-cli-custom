use super::*;
use crate::app::test_support::make_test_app;
use crate::app_event_sender::AppEventSender;
use tokio::sync::mpsc::unbounded_channel;

fn completed(thread_id: ThreadId, success: bool) -> ItemCompletedNotification {
    ItemCompletedNotification {
        item: ThreadItem::DynamicToolCall {
            id: "call".to_string(),
            namespace: None,
            tool: "threadClear".to_string(),
            arguments: serde_json::json!({}),
            status: DynamicToolCallStatus::Completed,
            content_items: None,
            success: Some(success),
            duration_ms: Some(1),
        },
        thread_id: thread_id.to_string(),
        turn_id: "turn".to_string(),
        completed_at_ms: 0,
    }
}

#[tokio::test]
async fn successful_exact_item_completion_defers_clear_until_completion() {
    let mut app = make_test_app().await;
    let (tx, mut rx) = unbounded_channel();
    app.app_event_tx = AppEventSender::new(tx);
    let thread_id = ThreadId::new();
    app.primary_thread_id = Some(thread_id);
    app.active_thread_id = Some(thread_id);
    app.pending_dynamic_thread_control = Some(PendingDynamicThreadControl {
        thread_id,
        turn_id: "turn".to_string(),
        call_id: "call".to_string(),
        tool: "threadClear".to_string(),
        control: DynamicThreadControl::Clear,
    });

    assert!(app.handle_dynamic_thread_control_completed(&completed(thread_id, true)));
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::ClearUi { name: None })
    ));
}

#[tokio::test]
async fn failed_item_completion_never_transitions_ui() {
    let mut app = make_test_app().await;
    let (tx, mut rx) = unbounded_channel();
    app.app_event_tx = AppEventSender::new(tx);
    let thread_id = ThreadId::new();
    app.primary_thread_id = Some(thread_id);
    app.active_thread_id = Some(thread_id);
    app.pending_dynamic_thread_control = Some(PendingDynamicThreadControl {
        thread_id,
        turn_id: "turn".to_string(),
        call_id: "call".to_string(),
        tool: "threadClear".to_string(),
        control: DynamicThreadControl::Clear,
    });

    assert!(app.handle_dynamic_thread_control_completed(&completed(thread_id, false)));
    assert!(rx.try_recv().is_err());
    assert!(app.pending_dynamic_thread_control.is_none());
}
