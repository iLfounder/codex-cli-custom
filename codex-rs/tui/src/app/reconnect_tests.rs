use super::canonical_reconnect::ConnectedDisposition;
use super::canonical_reconnect::ReconnectState;
use super::session_lifecycle::ThreadAttachPresentation;
use super::test_support::make_test_app_with_channels;
use crate::AppServerTarget;
use crate::app_command::AppCommand;
use crate::app_event::AppEvent;
use crate::canonical_launch_projection::CanonicalLaunchProjection;
use codex_app_server_client::AppServerInstanceIdentity;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use uuid::Uuid;

fn identity(discriminator: u128, generation: u64) -> AppServerInstanceIdentity {
    AppServerInstanceIdentity {
        instance_id: Uuid::from_u128(discriminator),
        generation,
    }
}

#[test]
fn reconnect_state_accepts_exact_identity_only_while_disconnected() {
    let mut state = ReconnectState::default();
    let baseline = identity(1, 4);
    assert!(matches!(
        state.observe_connected(baseline),
        ConnectedDisposition::Baseline
    ));
    assert!(matches!(
        state.observe_connected(baseline),
        ConnectedDisposition::Stale
    ));
    state.begin_disconnect(Some(ThreadId::new()), /*input_state*/ None);
    assert!(matches!(
        state.observe_connected(identity(2, 4)),
        ConnectedDisposition::Stale
    ));
    assert!(matches!(
        state.observe_connected(identity(3, 3)),
        ConnectedDisposition::Stale
    ));
    assert!(matches!(
        state.observe_connected(baseline),
        ConnectedDisposition::Resync(_)
    ));
    assert!(matches!(
        state.observe_connected(baseline),
        ConnectedDisposition::Stale
    ));
    state.begin_disconnect(Some(ThreadId::new()), /*input_state*/ None);
    assert!(matches!(
        state.observe_connected(identity(4, 5)),
        ConnectedDisposition::Resync(_)
    ));
}

#[tokio::test]
async fn supervised_disconnect_preserves_draft_and_reports_once() {
    let (mut app, mut event_rx, _op_rx) = make_test_app_with_channels().await;
    app.app_server_target = AppServerTarget::LocalDaemon {
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::from_absolute_path("/tmp/canonical-app.sock")
                .expect("absolute socket path"),
        },
        canonical_projection: Some(CanonicalLaunchProjection::default()),
    };
    app.chat_widget.insert_str("draft survives restart");

    app.handle_supervised_disconnect("App server connection was interrupted.".to_string());
    app.handle_supervised_disconnect("duplicate disconnect".to_string());

    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "draft survives restart"
    );
    assert!(app.chat_widget.composer_input_enabled());
    assert!(app.reconnect.offline);
    assert!(app.reconnect_state.is_disconnected());
    let cell = match event_rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => cell,
        other => panic!("expected one reconnect history cell, got {other:?}"),
    };
    let rendered = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @"■ Connection lost. Attempting to reconnect…");
    assert!(event_rx.try_recv().is_err());
}

#[tokio::test]
async fn same_identity_reconnect_replaces_unmaterialized_thread_without_sending_draft()
-> color_eyre::Result<()> {
    let (mut app, mut event_rx, mut op_rx) = make_test_app_with_channels().await;
    let mut app_server =
        crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let started = app_server.start_thread(&app.config).await?;
    let thread_id = started.session.thread_id;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.replace_chat_widget_with_app_server_thread(
        &mut tui,
        started,
        ThreadAttachPresentation::SessionLineage,
        /*initial_user_message*/ None,
    )
    .await?;
    while op_rx.try_recv().is_ok() {}
    app.app_server_target = AppServerTarget::LocalDaemon {
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::from_absolute_path("/tmp/canonical-app.sock")?,
        },
        canonical_projection: Some(CanonicalLaunchProjection::default()),
    };
    assert!(matches!(
        app.reconnect_state.observe_connected(identity(1, 1)),
        ConnectedDisposition::Baseline
    ));
    app.chat_widget
        .set_queue_autosend_suppressed(/*suppressed*/ true);
    app.chat_widget
        .restore_user_message_to_composer("keep this queued input".into());
    app.chat_widget.handle_key_event(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    let old_sender = app.app_event_tx.clone();
    app.chat_widget.insert_str("do not autosend this draft");
    app.handle_supervised_disconnect("connection interrupted".to_string());
    app.chat_widget.insert_str(" edited offline");

    app.handle_supervised_connected(&mut tui, &mut app_server, &mut event_rx, identity(1, 1))
        .await;

    assert!(app.current_displayed_thread_id().is_some());
    assert_ne!(app.current_displayed_thread_id(), Some(thread_id));
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "do not autosend this draft edited offline"
    );
    assert!(app.chat_widget.has_queued_follow_up_messages());
    assert!(app.chat_widget.capture_thread_input_state().unwrap().recovered_queue);
    assert!(old_sender.app_event_tx.is_closed());
    let reconnect_events = std::iter::from_fn(|| event_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(!reconnect_events.iter().any(|event| matches!(
        event,
        AppEvent::CodexOp(AppCommand::UserTurn { .. })
            | AppEvent::SubmitThreadOp { op: AppCommand::UserTurn { .. }, .. }
    )));
    let reconnect_messages = reconnect_events
        .into_iter()
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(
                cell.display_lines(/*width*/ 80)
                    .into_iter()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !app.reconnect_state.is_disconnected(),
        "resync remained pending: {reconnect_messages:?}"
    );
    assert!(app.chat_widget.composer_input_enabled());
    assert!(
        std::iter::from_fn(|| op_rx.try_recv().ok())
            .all(|op| !matches!(op, AppCommand::UserTurn { .. }))
    );
    app_server.shutdown().await?;
    Ok(())
}
