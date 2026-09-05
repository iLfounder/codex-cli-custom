use std::time::Duration;

use codex_app_server_protocol::PluginUninstallResponse;
use pretty_assertions::assert_eq;

use super::*;

fn set_remote_plugin_workspace(app: &mut App, thread_id: ThreadId, cwd: &str) {
    let mut session =
        test_thread_session(thread_id, app.chat_widget.config_ref().cwd.to_path_buf());
    session.execution_context = crate::session_state::SessionExecutionContext::Remote {
        cwd: LegacyAppPathString::from_string(cwd),
        runtime_workspace_roots: Vec::new(),
        sandbox: codex_app_server_protocol::SandboxPolicy::DangerFullAccess,
        rollout_path: None,
    };
    app.chat_widget.handle_thread_session(session);
}

#[tokio::test]
async fn workspace_aba_drops_old_catalog_work_without_clearing_current_flight() -> Result<()> {
    let (mut app, mut rx, _op_rx) = make_test_app_with_channels().await;
    app.chat_widget
        .set_feature_enabled(Feature::Plugins, /*enabled*/ true);
    let cwd = app.chat_widget.config_ref().cwd.to_path_buf();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut server = Box::pin(crate::start_embedded_app_server_for_picker(
        app.chat_widget.config_ref(),
    ))
    .await?;
    let thread_id = ThreadId::new();
    set_remote_plugin_workspace(&mut app, thread_id, "/remote/a");
    let old_scope = app.chat_widget.workspace_request_scope();
    set_remote_plugin_workspace(&mut app, thread_id, "/remote/b");
    set_remote_plugin_workspace(&mut app, thread_id, "/remote/a");
    let current_scope = app.chat_widget.workspace_request_scope();
    assert_eq!(old_scope.cwd, current_scope.cwd);
    assert_ne!(old_scope.generation, current_scope.generation);
    assert_eq!(app.chat_widget.config_ref().cwd.as_path(), cwd.as_path());
    app.chat_widget.on_plugins_list_fetch_started(cwd.clone());
    while rx.try_recv().is_ok() {}

    let stale_events = [
        AppEvent::FetchPluginsList {
            scope: old_scope.clone(),
            cwd: cwd.clone(),
        },
        AppEvent::FetchHooksList {
            scope: old_scope.clone(),
            cwd: cwd.clone(),
        },
        AppEvent::PluginsLoaded {
            marketplace_management: None,
            scope: old_scope.clone(),
            cwd: cwd.clone(),
            result: Err("old catalog".into()),
        },
        AppEvent::PluginRemoteSectionsLoaded {
            scope: old_scope.clone(),
            cwd: cwd.clone(),
            marketplaces: Vec::new(),
            section_errors: Vec::new(),
        },
        AppEvent::HooksLoaded {
            scope: old_scope.clone(),
            cwd: cwd.clone(),
            result: Err("old hooks".into()),
        },
        AppEvent::SkillsListLoaded {
            scope: old_scope.clone(),
            cwd: cwd.clone(),
            result: Err("old skills".into()),
        },
        AppEvent::PluginMentionsLoaded {
            scope: old_scope.clone(),
            cwd: cwd.clone(),
            plugins: None,
        },
        AppEvent::DiffResult {
            scope: old_scope,
            text: "old diff".into(),
        },
    ];
    for event in stale_events {
        app.handle_event(&mut tui, &mut server, event).await?;
    }
    assert!(
        rx.try_recv().is_err(),
        "stale work must not start fetches or publish errors"
    );
    assert!(app.overlay.is_none());
    app.chat_widget.add_plugins_output();
    assert!(
        rx.try_recv().is_err(),
        "old completion must retain the current flight"
    );

    app.handle_event(
        &mut tui,
        &mut server,
        AppEvent::PluginsLoaded {
            marketplace_management: None,
            scope: current_scope.clone(),
            cwd: cwd.clone(),
            result: Ok(codex_app_server_protocol::PluginListResponse {
                marketplaces: Vec::new(),
                marketplace_load_errors: Vec::new(),
                featured_plugin_ids: Vec::new(),
            }),
        },
    )
    .await?;
    app.chat_widget.add_plugins_output();
    assert!(
        matches!(rx.try_recv(), Ok(AppEvent::FetchPluginsList { scope, .. }) if scope == current_scope)
    );
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stale_toggle_completion_releases_only_its_own_workspace_write() -> Result<()> {
    let (mut app, mut rx, _op_rx) = make_test_app_with_channels().await;
    let cwd = app.chat_widget.config_ref().cwd.to_path_buf();
    let old_scope = app.chat_widget.workspace_request_scope();
    app.chat_widget.invalidate_connector_scope();
    let current_scope = app.chat_widget.workspace_request_scope();
    let plugin_id = "plugin-fixture".to_string();
    let hook_key = "hook-fixture".to_string();
    for scope in [&old_scope, &current_scope] {
        app.pending_plugin_enabled_writes
            .insert((scope.clone(), plugin_id.clone()), None);
        app.pending_hook_enabled_writes
            .insert((scope.clone(), hook_key.clone()), None);
    }
    while rx.try_recv().is_ok() {}
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut server = Box::pin(crate::start_embedded_app_server_for_picker(
        app.chat_widget.config_ref(),
    ))
    .await?;
    for event in [
        AppEvent::PluginEnabledSet {
            scope: old_scope.clone(),
            cwd,
            plugin_id: plugin_id.clone(),
            enabled: true,
            result: Err("old write failed".into()),
        },
        AppEvent::HookEnabledSet {
            scope: old_scope.clone(),
            key: hook_key.clone(),
            enabled: true,
            result: Err("old write failed".into()),
        },
    ] {
        app.handle_event(&mut tui, &mut server, event).await?;
    }
    assert_eq!(app.pending_plugin_enabled_writes.len(), 1);
    assert!(
        app.pending_plugin_enabled_writes
            .contains_key(&(current_scope.clone(), plugin_id))
    );
    assert_eq!(app.pending_hook_enabled_writes.len(), 1);
    assert!(
        app.pending_hook_enabled_writes
            .contains_key(&(current_scope, hook_key))
    );
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn completed_write_from_previous_scope_is_reported_without_reopening_its_popup() {
    let (mut app, mut rx, _op_rx) = make_test_app_with_channels().await;
    let mut scope = app.chat_widget.workspace_request_scope();
    scope.cwd = LegacyAppPathString::from_string("/previous/workspace");
    while rx.try_recv().is_ok() {}
    assert!(app.report_stale_workspace_mutation(&scope, "Plugin installation", Ok(())));
    let mut messages = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            messages.push(lines_to_single_string(&cell.display_lines(/*width*/ 120)));
        }
    }
    insta::assert_snapshot!(messages.join("\n"), @"• Plugin installation completed in the previous workspace (/previous/workspace).");
}

#[tokio::test]
async fn successful_plugin_uninstall_dispatches_plugin_list_refresh() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let cwd = app.chat_widget.config_ref().cwd.to_path_buf();
    while app_event_rx.try_recv().is_ok() {}

    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(
        app.chat_widget.config_ref(),
    ))
    .await?;
    let scope = app.chat_widget.workspace_request_scope();
    let control = Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::PluginUninstallLoaded {
            scope,
            cwd: cwd.clone(),
            plugin_id: "plugin-docs".to_string(),
            plugin_display_name: "Docs".to_string(),
            result: Ok(PluginUninstallResponse {}),
        },
    ))
    .await?;
    assert!(matches!(control, AppRunControl::Continue));

    let refresh_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match app_event_rx.recv().await {
                Some(AppEvent::PluginsLoaded {
                    cwd: event_cwd,
                    result,
                    ..
                }) if event_cwd == cwd => break result,
                Some(_) => {}
                None => panic!("app event channel closed before plugin refresh completed"),
            }
        }
    })
    .await
    .expect("dispatcher should initiate a plugin list refresh");
    refresh_result.expect("plugin list refresh should succeed");

    app_server.shutdown().await?;
    Ok(())
}
