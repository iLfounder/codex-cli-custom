//! Focused regression coverage for per-thread routing state.

use super::*;
use codex_app_server_protocol::ThreadNameUpdatedNotification;
use pretty_assertions::assert_eq;

fn thread_name_update(thread_id: ThreadId, name: &str) -> ServerNotification {
    ServerNotification::ThreadNameUpdated(ThreadNameUpdatedNotification {
        thread_id: thread_id.to_string(),
        thread_name: Some(name.to_string()),
    })
}

async fn cached_thread_name(app: &App, thread_id: ThreadId) -> Option<String> {
    let channel = app
        .thread_event_channels
        .get(&thread_id)
        .expect("thread channel");
    let store = channel.store.lock().await;
    store
        .session
        .as_ref()
        .and_then(|session| session.thread_name.clone())
}

#[tokio::test]
async fn active_thread_name_is_cached_when_the_notification_is_routed() -> Result<()> {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    let mut session = test_thread_session(thread_id, test_path_buf("/tmp/main"));
    session.thread_name = Some("Old active name".to_string());
    app.primary_thread_id = Some(thread_id);
    app.primary_session_configured = Some(session.clone());
    app.thread_event_channels.insert(
        thread_id,
        ThreadEventChannel::new_with_session(/*capacity*/ 4, session.clone(), Vec::new()),
    );
    app.active_thread_id = None;
    app.activate_thread_channel(thread_id).await;
    app.chat_widget.handle_thread_session(session);

    app.enqueue_thread_notification(
        thread_id,
        thread_name_update(thread_id, "Accepted active name"),
    )
    .await?;

    assert_eq!(
        app.primary_session_configured
            .as_ref()
            .and_then(|session| session.thread_name.as_deref()),
        Some("Accepted active name")
    );
    assert_eq!(
        cached_thread_name(&app, thread_id).await.as_deref(),
        Some("Accepted active name")
    );
    Ok(())
}

#[tokio::test]
async fn inactive_thread_name_survives_switch_replay_and_footer_sync() -> Result<()> {
    let mut app = make_test_app().await;
    let active_thread_id = ThreadId::new();
    app.active_thread_id = Some(active_thread_id);
    let inactive_thread_id = ThreadId::new();
    let mut inactive_session =
        test_thread_session(inactive_thread_id, test_path_buf("/tmp/inactive"));
    inactive_session.thread_name = Some("Stale cached name".to_string());
    app.thread_event_channels.insert(
        inactive_thread_id,
        ThreadEventChannel::new_with_session(/*capacity*/ 4, inactive_session, Vec::new()),
    );

    app.enqueue_thread_notification(
        inactive_thread_id,
        thread_name_update(inactive_thread_id, "Accepted inactive name"),
    )
    .await?;
    assert_eq!(
        cached_thread_name(&app, inactive_thread_id)
            .await
            .as_deref(),
        Some("Accepted inactive name")
    );

    app.active_thread_id = None;
    let (receiver, snapshot) = app
        .activate_thread_for_replay(inactive_thread_id)
        .await
        .expect("inactive thread should activate for replay");
    app.active_thread_id = Some(inactive_thread_id);
    app.active_thread_rx = Some(receiver);
    app.replay_thread_snapshot(snapshot, /*resume_restored_queue*/ false);

    assert_eq!(
        app.chat_widget.thread_name().as_deref(),
        Some("Accepted inactive name")
    );
    app.sync_footer_runtime_projection_for_thread(inactive_thread_id);
    assert_eq!(
        app.chat_widget.thread_name().as_deref(),
        Some("Accepted inactive name")
    );
    Ok(())
}
