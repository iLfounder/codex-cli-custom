use super::*;
use crate::app_event::RateLimitRequestSubject;
use codex_app_server_protocol::AccountRateLimitsUpdatedNotification;
use codex_app_server_protocol::CodexErrorInfo;
use codex_app_server_protocol::CreditsSnapshot;
use codex_app_server_protocol::ErrorNotification;
use codex_app_server_protocol::GetAccountRateLimitsResponse;
use codex_app_server_protocol::RateLimitReachedType;
use codex_app_server_protocol::RateLimitResetCreditsSummary;
use codex_app_server_protocol::RateLimitSnapshot;
use codex_app_server_protocol::RateLimitWindow;
use codex_app_server_protocol::SessionRuntimeAccountRef;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use pretty_assertions::assert_eq;

fn rate_limit_snapshot(
    used_percent: i32,
    rate_limit_reached_type: Option<RateLimitReachedType>,
    spend_control_reached: Option<bool>,
) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent,
            window_duration_mins: Some(300),
            resets_at: None,
        }),
        secondary: None,
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: false,
            balance: None,
        }),
        individual_limit: None,
        spend_control_reached,
        plan_type: None,
        rate_limit_reached_type,
    }
}

fn rate_limit_subject(app: &App) -> RateLimitRequestSubject {
    RateLimitRequestSubject {
        thread_id: app
            .current_displayed_thread_id()
            .expect("displayed test thread"),
        execution_account: Some(SessionRuntimeAccountRef {
            account_slot_id: "default".to_string(),
            execution_generation: 0,
        }),
    }
}

fn install_rate_limit_subject(app: &mut App) -> RateLimitRequestSubject {
    if app.current_displayed_thread_id().is_none() {
        app.active_thread_id = Some(ThreadId::new());
    }
    let subject = rate_limit_subject(app);
    app.account_runtime = Some((
        "instance".to_string(),
        serde_json::from_value(serde_json::json!({
            "threadId": subject.thread_id.to_string(),
            "stateRevision": 1,
            "identity": {
                "sessionId": subject.thread_id.to_string(),
                "forkedFromId": null,
                "parentThreadId": null,
                "name": null,
                "source": "test",
                "cwd": "/tmp",
                "gitInfo": null,
                "settings": null
            },
            "lifecycle": {
                "state": "idle",
                "activeTurnId": null,
                "waitingOn": [],
                "subscriberCount": 1,
                "clientIncarnations": [],
                "lastActivityAt": null,
                "unloadAt": null
            },
            "writer": {
                "state": "ownedHere",
                "storeId": null,
                "writerGeneration": 1,
                "denyReason": null
            },
            "persistence": {
                "jsonl": null,
                "sqlite": null,
                "lag": null,
                "flushHealth": "unknown",
                "materializeHealth": "unknown",
                "flushedAt": null,
                "materializedAt": null,
                "denyReason": null
            },
            "account": {
                "current": subject.execution_account,
                "activeTurn": null,
                "switchState": "stable",
                "switchTargetSlotId": null,
                "denyReason": null
            },
            "actions": []
        }))
        .expect("runtime snapshot"),
    ));
    subject
}

fn account_rate_limits_response(
    subject: &RateLimitRequestSubject,
    snapshot: RateLimitSnapshot,
) -> GetAccountRateLimitsResponse {
    GetAccountRateLimitsResponse {
        thread_id: Some(subject.thread_id.to_string()),
        execution_account: subject.execution_account.clone(),
        rate_limits: snapshot,
        rate_limits_by_limit_id: None,
        rate_limit_reset_credits: Some(RateLimitResetCreditsSummary {
            available_count: 0,
            credits: None,
        }),
    }
}

async fn deliver_rolling_rate_limit_snapshot(
    app: &mut App,
    app_server: &AppServerSession,
    snapshot: RateLimitSnapshot,
) {
    let subject = install_rate_limit_subject(app);
    app.handle_app_server_event(
        app_server,
        codex_app_server_client::AppServerEvent::ServerNotification(Box::new(
            ServerNotification::AccountRateLimitsUpdated(AccountRateLimitsUpdatedNotification {
                thread_id: Some(subject.thread_id.to_string()),
                execution_account: subject.execution_account,
                rate_limits: snapshot,
            }),
        )),
    )
    .await;
}

fn render_status_output(
    app: &mut App,
    app_event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> String {
    while app_event_rx.try_recv().is_ok() {}
    app.chat_widget.add_status_output(
        /*refreshing_rate_limits*/ false, /*request_id*/ None,
    );
    match app_event_rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => cell
            .display_lines(/*width*/ 120)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("expected status output, got {other:?}"),
    }
}

fn deliver_usage_limit_error(app: &mut App) {
    app.chat_widget.handle_server_notification(
        ServerNotification::Error(ErrorNotification {
            error: AppServerTurnError {
                message: "Usage limit reached.".to_string(),
                codex_error_info: Some(CodexErrorInfo::UsageLimitExceeded),
                additional_details: None,
            },
            will_retry: false,
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
        }),
        /*replay_kind*/ None,
    );
}

#[tokio::test]
async fn rolling_workspace_hard_stops_invalidate_older_rate_limit_reads() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let app_server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref())
        .await
        .expect("embedded app server");

    let cases = [
        (None, None, false),
        (Some(RateLimitReachedType::RateLimitReached), None, false),
        (None, Some(false), false),
        (None, Some(true), true),
        (
            Some(RateLimitReachedType::WorkspaceOwnerCreditsDepleted),
            None,
            true,
        ),
        (
            Some(RateLimitReachedType::WorkspaceMemberCreditsDepleted),
            None,
            true,
        ),
        (
            Some(RateLimitReachedType::WorkspaceOwnerUsageLimitReached),
            None,
            true,
        ),
        (
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
            None,
            true,
        ),
    ];
    let mut expected_generation = 0;
    for (reached_type, spend_control_reached, invalidates) in cases {
        deliver_rolling_rate_limit_snapshot(
            &mut app,
            &app_server,
            rate_limit_snapshot(
                /*used_percent*/ 95,
                reached_type,
                spend_control_reached,
            ),
        )
        .await;
        if invalidates {
            expected_generation += 1;
        }
        assert_eq!(
            app.rate_limit_hard_stop_generation, expected_generation,
            "reached_type={reached_type:?}, spend_control_reached={spend_control_reached:?}"
        );
    }

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn mismatched_rolling_rate_limit_subject_changes_neither_cache_nor_hard_stop() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    set_chatgpt_auth(&mut app.chat_widget);
    let app_server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref())
        .await
        .expect("embedded app server");

    deliver_rolling_rate_limit_snapshot(
        &mut app,
        &app_server,
        rate_limit_snapshot(
            /*used_percent*/ 40,
            /*rate_limit_reached_type*/ None,
            Some(false),
        ),
    )
    .await;
    let generation = app.rate_limit_hard_stop_generation;
    assert!(render_status_output(&mut app, &mut app_event_rx).contains("60% left"));

    let subject = rate_limit_subject(&app);
    app.handle_app_server_event(
        &app_server,
        codex_app_server_client::AppServerEvent::ServerNotification(Box::new(
            ServerNotification::AccountRateLimitsUpdated(AccountRateLimitsUpdatedNotification {
                thread_id: Some(subject.thread_id.to_string()),
                execution_account: Some(SessionRuntimeAccountRef {
                    account_slot_id: "default".to_string(),
                    execution_generation: 1,
                }),
                rate_limits: rate_limit_snapshot(
                    /*used_percent*/ 99,
                    Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
                    Some(true),
                ),
            }),
        )),
    )
    .await;

    assert_eq!(app.rate_limit_hard_stop_generation, generation);
    let status = render_status_output(&mut app, &mut app_event_rx);
    assert!(status.contains("60% left"), "unexpected status: {status}");
    assert!(!status.contains("1% left"), "unexpected status: {status}");

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stale_rate_limit_reads_preserve_newer_workspace_hard_stop_for_every_origin() -> Result<()>
{
    for origin_name in [
        "startup",
        "status",
        "usage",
        "reset-picker",
        "reset-consume",
    ] {
        let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
        set_chatgpt_auth(&mut app.chat_widget);
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(
            app.chat_widget.config_ref(),
        ))
        .await?;

        let origin = match origin_name {
            "startup" => RateLimitRefreshOrigin::StartupPrefetch {
                reset_hint_request_id: app.chat_widget.start_rate_limit_reset_startup_check(),
            },
            "status" => {
                let request_id = 7;
                app.chat_widget
                    .add_status_output(/*refreshing_rate_limits*/ true, Some(request_id));
                RateLimitRefreshOrigin::StatusCommand { request_id }
            }
            "usage" => {
                let startup_request_id = app.chat_widget.start_rate_limit_reset_startup_check();
                app.chat_widget.finish_rate_limit_reset_hint_refresh(
                    startup_request_id,
                    Vec::new(),
                    Ok(RateLimitResetCreditsSummary {
                        available_count: 0,
                        credits: None,
                    }),
                );
                app.chat_widget.insert_str("/usage");
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                loop {
                    match app_event_rx.try_recv() {
                        Ok(AppEvent::RefreshRateLimits { origin }) => break origin,
                        Ok(_) => {}
                        other => panic!("expected usage refresh request, got {other:?}"),
                    }
                }
            }
            "reset-picker" => RateLimitRefreshOrigin::ResetPicker {
                request_id: app.chat_widget.show_rate_limit_reset_loading_popup(),
            },
            "reset-consume" => RateLimitRefreshOrigin::ResetConsume {
                request_id: app.chat_widget.show_rate_limit_reset_consuming_popup(),
            },
            _ => unreachable!("unknown refresh origin"),
        };
        let read_generation = app.rate_limit_hard_stop_generation;
        let mut rolling_snapshot = rate_limit_snapshot(
            /*used_percent*/ 95,
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
            Some(true),
        );
        if origin_name == "reset-picker" {
            rolling_snapshot.limit_id = Some("codex_other".to_string());
        }
        deliver_rolling_rate_limit_snapshot(&mut app, &app_server, rolling_snapshot).await;
        assert_ne!(read_generation, app.rate_limit_hard_stop_generation);
        let subject = rate_limit_subject(&app);

        let control = Box::pin(app.handle_event(
            &mut tui,
            &mut app_server,
            AppEvent::RateLimitsLoaded {
                origin,
                subject: Some(subject.clone()),
                hard_stop_generation: read_generation,
                result: Ok(account_rate_limits_response(
                    &subject,
                    rate_limit_snapshot(
                        /*used_percent*/ 0,
                        /*rate_limit_reached_type*/ None,
                        Some(false),
                    ),
                )),
            },
        ))
        .await?;
        assert!(matches!(control, AppRunControl::Continue));

        let popup = render_bottom_popup(&app.chat_widget, /*width*/ 100);
        match origin_name {
            "usage" => assert!(popup.contains("No usage limit resets available.")),
            "reset-picker" => {
                assert!(popup.contains("You don't have any usage limit resets available."));
            }
            "reset-consume" => {
                assert!(popup.contains("Usage reset. You have 0 usage limit resets left."));
            }
            "startup" | "status" => {}
            _ => unreachable!("unknown refresh origin"),
        }

        let status = render_status_output(&mut app, &mut app_event_rx);
        assert!(
            status.contains("5% left"),
            "expected {origin_name} to preserve rolling limits, got: {status}"
        );
        deliver_usage_limit_error(&mut app);
        let popup = render_bottom_popup(&app.chat_widget, /*width*/ 100);
        assert!(
            popup.contains("Request a limit increase from your owner"),
            "expected {origin_name} to preserve workspace error routing, got: {popup}"
        );

        app_server.shutdown().await?;
    }

    Ok(())
}

#[tokio::test]
async fn stale_rate_limit_read_does_not_dismiss_visible_workspace_advisory() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    set_chatgpt_auth(&mut app.chat_widget);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(
        app.chat_widget.config_ref(),
    ))
    .await?;
    let request_id = 7;
    app.chat_widget
        .add_status_output(/*refreshing_rate_limits*/ true, Some(request_id));
    let read_generation = app.rate_limit_hard_stop_generation;

    deliver_rolling_rate_limit_snapshot(
        &mut app,
        &app_server,
        rate_limit_snapshot(
            /*used_percent*/ 95,
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
            Some(true),
        ),
    )
    .await;
    app.chat_widget.handle_server_notification(
        turn_completed_notification(ThreadId::new(), "turn-1", TurnStatus::Completed),
        /*replay_kind*/ None,
    );
    assert!(
        render_bottom_popup(&app.chat_widget, /*width*/ 100).contains("Approaching rate limits")
    );
    let subject = rate_limit_subject(&app);

    Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::RateLimitsLoaded {
            origin: RateLimitRefreshOrigin::StatusCommand { request_id },
            subject: Some(subject.clone()),
            hard_stop_generation: read_generation,
            result: Ok(account_rate_limits_response(
                &subject,
                rate_limit_snapshot(
                    /*used_percent*/ 0,
                    /*rate_limit_reached_type*/ None,
                    Some(false),
                ),
            )),
        },
    ))
    .await?;

    assert!(
        render_bottom_popup(&app.chat_widget, /*width*/ 100).contains("Approaching rate limits")
    );
    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn post_hard_stop_rate_limit_read_clears_recovered_workspace_limit() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    set_chatgpt_auth(&mut app.chat_widget);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(
        app.chat_widget.config_ref(),
    ))
    .await?;
    deliver_rolling_rate_limit_snapshot(
        &mut app,
        &app_server,
        rate_limit_snapshot(
            /*used_percent*/ 95,
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
            Some(true),
        ),
    )
    .await;
    let read_generation = app.rate_limit_hard_stop_generation;
    let request_id = 7;
    app.chat_widget
        .add_status_output(/*refreshing_rate_limits*/ true, Some(request_id));
    let subject = rate_limit_subject(&app);

    let control = Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::RateLimitsLoaded {
            origin: RateLimitRefreshOrigin::StatusCommand { request_id },
            subject: Some(subject.clone()),
            hard_stop_generation: read_generation,
            result: Ok(account_rate_limits_response(
                &subject,
                rate_limit_snapshot(
                    /*used_percent*/ 0,
                    /*rate_limit_reached_type*/ None,
                    Some(false),
                ),
            )),
        },
    ))
    .await?;
    assert!(matches!(control, AppRunControl::Continue));

    let status = render_status_output(&mut app, &mut app_event_rx);
    assert!(
        status.contains("100% left"),
        "expected recovered limits, got: {status}"
    );
    deliver_usage_limit_error(&mut app);
    let popup = render_bottom_popup(&app.chat_widget, /*width*/ 100);
    assert!(
        !popup.contains("Request a limit increase from your owner"),
        "expected recovered state to clear workspace error routing, got: {popup}"
    );

    app_server.shutdown().await?;
    Ok(())
}
