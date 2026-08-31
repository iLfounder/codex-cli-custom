use super::*;
use crate::app::test_support::make_test_app;
use crate::app_event_sender::AppEventSender;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStatus;
use tokio::sync::mpsc::UnboundedReceiver;
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

fn turn_completed(thread_id: ThreadId, status: TurnStatus) -> TurnCompletedNotification {
    TurnCompletedNotification {
        thread_id: thread_id.to_string(),
        turn: Turn {
            id: "turn".to_string(),
            items: Vec::new(),
            items_view: Default::default(),
            status,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        },
    }
}

fn assert_no_transition(rx: &mut UnboundedReceiver<AppEvent>) {
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn successful_item_waits_for_completed_turn() {
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
        item_completed: false,
    });

    assert!(app.handle_dynamic_thread_control_completed(&completed(thread_id, true)));
    assert_no_transition(&mut rx);
    assert!(
        app.handle_dynamic_thread_control_turn_completed(&turn_completed(
            thread_id,
            TurnStatus::Completed,
        ))
    );
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
        item_completed: false,
    });

    assert!(app.handle_dynamic_thread_control_completed(&completed(thread_id, false)));
    assert_no_transition(&mut rx);
    assert!(app.pending_dynamic_thread_control.is_none());
}

#[tokio::test]
async fn terminal_without_successful_item_cancels_pending_control() {
    for status in [
        TurnStatus::Completed,
        TurnStatus::Failed,
        TurnStatus::Interrupted,
    ] {
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
            item_completed: status != TurnStatus::Completed,
        });

        assert!(
            app.handle_dynamic_thread_control_turn_completed(&turn_completed(thread_id, status,))
        );
        assert_no_transition(&mut rx);
        assert!(app.pending_dynamic_thread_control.is_none());
    }
}
