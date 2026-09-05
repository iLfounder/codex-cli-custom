use super::*;
use crate::app::test_support::make_test_app_with_channels;
use pretty_assertions::assert_eq;

async fn assert_stale_account_inventory_preserves_current_request(account_transitions: usize) {
    let (mut app, mut rx, _op_rx) = make_test_app_with_channels().await;
    let thread_id = ThreadId::new();
    app.active_thread_id = Some(thread_id);
    let old_scope = app.chat_widget.workspace_request_scope();

    // Account runtime changes use this same invalidation boundary. A return to
    // account A must not revive its earlier request even at the same thread/cwd.
    for _ in 0..account_transitions {
        app.chat_widget.invalidate_connector_scope();
    }
    let current_scope = app.chat_widget.workspace_request_scope();
    assert_eq!(old_scope.cwd, current_scope.cwd);
    assert_ne!(old_scope.generation, current_scope.generation);

    app.chat_widget
        .add_mcp_output(McpServerStatusDetail::ToolsAndAuthOnly);
    app.transcript_cells
        .push(Arc::new(history_cell::new_mcp_inventory_loading(
            /*animations_enabled*/ false,
        )));
    while rx.try_recv().is_ok() {}
    let loading_revision = app
        .chat_widget
        .active_cell_transcript_key()
        .expect("current MCP request should show its spinner")
        .revision;

    for result in [
        Ok(Vec::new()),
        Err("old account inventory error".to_string()),
    ] {
        app.handle_mcp_inventory_result(
            old_scope.clone(),
            result,
            McpServerStatusDetail::ToolsAndAuthOnly,
            Some(thread_id),
        );
        assert_eq!(
            app.chat_widget
                .active_cell_transcript_key()
                .map(|key| key.revision),
            Some(loading_revision),
        );
        assert_eq!(app.transcript_cells.len(), 1);
        assert!(
            rx.try_recv().is_err(),
            "stale success and failure must not add inventory or errors to history"
        );
    }

    app.handle_mcp_inventory_result(
        current_scope,
        Ok(Vec::new()),
        McpServerStatusDetail::ToolsAndAuthOnly,
        Some(thread_id),
    );
    assert!(app.chat_widget.active_cell_transcript_key().is_none());
    assert!(app.transcript_cells.is_empty());
    assert!(matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_))));
}

#[tokio::test]
async fn same_thread_account_change_drops_stale_mcp_completion() {
    assert_stale_account_inventory_preserves_current_request(/*account_transitions*/ 1).await;
}

#[tokio::test]
async fn account_a_b_a_does_not_revive_old_mcp_completion() {
    assert_stale_account_inventory_preserves_current_request(/*account_transitions*/ 2).await;
}

#[tokio::test]
async fn scope_change_ends_mcp_spinner_without_a_replacement_request() {
    let (mut app, mut rx, _op_rx) = make_test_app_with_channels().await;
    let old_scope = app.chat_widget.workspace_request_scope();
    app.chat_widget
        .add_mcp_output(McpServerStatusDetail::ToolsAndAuthOnly);
    assert!(app.chat_widget.active_cell_transcript_key().is_some());
    for _ in 0..2 {
        app.transcript_cells
            .push(Arc::new(history_cell::new_mcp_inventory_loading(
                /*animations_enabled*/ false,
            )));
    }
    let retained: Arc<dyn history_cell::HistoryCell> = Arc::new(history_cell::empty_mcp_output());
    app.transcript_cells.push(Arc::clone(&retained));
    while rx.try_recv().is_ok() {}

    app.clear_committed_mcp_inventory_on_scope_change();
    app.chat_widget.invalidate_connector_scope();

    assert!(app.chat_widget.active_cell_transcript_key().is_none());
    assert_eq!(app.transcript_cells.len(), 1);
    assert!(Arc::ptr_eq(&app.transcript_cells[0], &retained));
    app.handle_mcp_inventory_result(
        old_scope,
        Ok(Vec::new()),
        McpServerStatusDetail::ToolsAndAuthOnly,
        /*thread_id*/ None,
    );
    assert!(app.chat_widget.active_cell_transcript_key().is_none());
    assert!(rx.try_recv().is_err());
}
