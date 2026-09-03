use super::*;

use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

#[test]
fn default_footer_is_disabled_and_reserves_no_rows() {
    let footer = FooterBox::new(FooterBoxConfig::default());

    assert!(!footer.is_enabled());
    assert!(footer.model().rows.is_empty());
    assert_eq!(footer.measure(80).height, 0);
    assert_eq!(footer.desired_height(80), 0);
}

#[test]
fn snapshot_values_are_normalized_and_bounded() {
    let value = FooterStatusValue::new("  Status\n", "  hello\tworld  ").expect("value");
    assert_eq!(value.label, "Status");
    assert_eq!(value.value, "hello world");

    let long = "x".repeat(MAX_SNAPSHOT_TEXT_CHARS + 8);
    let value = FooterStatusValue::new("label", long).expect("bounded value");
    assert_eq!(value.value.chars().count(), MAX_SNAPSHOT_TEXT_CHARS + 1);
    assert_eq!(value.value.chars().last(), Some('…'));
}

#[test]
fn unknown_adapter_ids_are_ignored_without_changing_the_registry() {
    let config = FooterBoxConfig::default()
        .with_enabled(true)
        .with_adapter_ids(["does-not-exist"]);
    let snapshot = FooterSnapshot::new().with_account(
        Some("user@example.test".to_string()),
        Some("Plus".to_string()),
    );
    let footer = FooterBox::with_snapshot(config, snapshot);

    assert!(footer.model().rows.is_empty());
}

#[test]
fn bordered_layout_and_measurement_are_consistent() {
    let config = FooterBoxConfig {
        enabled: true,
        max_rows: 3,
        border: FooterBorderStyle::Rounded,
        layout: FooterLayoutStyle::Stacked,
        adapter_ids: Vec::new(),
        rows: Vec::new(),
        colors: BTreeMap::new(),
    };
    let snapshot = FooterSnapshot::new()
        .with_official_status(vec![FooterStatusValue::new("Status", "ready").unwrap()])
        .with_account(
            Some("user@example.test".to_string()),
            Some("Plus".to_string()),
        )
        .with_session_thread(
            Some("session-1".to_string()),
            None,
            Some("thread-1".to_string()),
            None,
        );
    let footer = FooterBox::with_snapshot(config, snapshot);
    let measured = footer.measure(48);
    let area = Rect::new(0, 0, measured.width, measured.height);
    let layout = footer.layout(area);

    assert_eq!(footer.desired_height(48), measured.height);
    assert_eq!(layout.rows.len() as u16, measured.rows);
    assert_eq!(layout.border, measured.border);
    assert_eq!(layout.content_area.height, measured.rows);

    let mut buffer = Buffer::empty(area);
    footer.render_layout(&layout, &mut buffer);
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| !cell.symbol().trim().is_empty())
    );
}

#[test]
fn tiny_areas_fall_back_to_a_single_borderless_row() {
    let config = FooterBoxConfig {
        enabled: true,
        max_rows: 4,
        border: FooterBorderStyle::Double,
        layout: FooterLayoutStyle::Stacked,
        adapter_ids: Vec::new(),
        rows: Vec::new(),
        colors: BTreeMap::new(),
    };
    let snapshot = FooterSnapshot::new().with_account(
        Some("user@example.test".to_string()),
        Some("Plus".to_string()),
    );
    let footer = FooterBox::with_snapshot(config, snapshot);
    let area = Rect::new(0, 0, 2, 1);
    let layout = footer.layout(area);

    assert_eq!(layout.border, FooterBorderStyle::None);
    assert!(layout.rows.len() <= 1);
    let mut buffer = Buffer::empty(area);
    footer.render_layout(&layout, &mut buffer);
}

#[test]
fn narrow_bordered_measurement_uses_the_same_borderless_fallback_as_layout() {
    let config = FooterBoxConfig {
        enabled: true,
        max_rows: 4,
        border: FooterBorderStyle::Rounded,
        layout: FooterLayoutStyle::Stacked,
        adapter_ids: Vec::new(),
        rows: Vec::new(),
        colors: BTreeMap::new(),
    };
    let snapshot = FooterSnapshot::new().with_account(
        Some("user@example.test".to_string()),
        Some("Plus".to_string()),
    );
    let footer = FooterBox::with_snapshot(config, snapshot);

    for width in [3, 4] {
        let measured = footer.measure(width);
        let area = Rect::new(0, 0, width, measured.height);
        let layout = footer.layout(area);

        assert_eq!(measured.border, FooterBorderStyle::None);
        assert_eq!(layout.border, measured.border);
        assert_eq!(layout.rows.len() as u16, measured.rows);
        assert_eq!(layout.content_area.width, measured.content_width);
    }
}

#[test]
fn runtime_projection_preserves_existing_fields_and_renders() {
    let config = FooterBoxConfig {
        enabled: true,
        max_rows: 8,
        border: FooterBorderStyle::Rounded,
        layout: FooterLayoutStyle::Stacked,
        adapter_ids: DEFAULT_ADAPTER_IDS.map(str::to_string).to_vec(),
        rows: Vec::new(),
        colors: BTreeMap::new(),
    };
    let snapshot = FooterSnapshot::new()
        .with_official_status(vec![FooterStatusValue::new("Status", "ready").unwrap()])
        .with_account(
            Some("user@example.test".to_string()),
            Some("Plus".to_string()),
        );
    let mut footer = FooterBox::with_snapshot(config, snapshot);
    footer.set_runtime_projection(FooterRuntimeProjection {
        managed_slot_label: Some("2".to_string()),
        managed_slot_id: Some("C2".to_string()),
        managed_slot_health: Some("healthy".to_string()),
        managed_slot_quota: Some("Week 68%".to_string()),
        session_id: Some("session-1".to_string()),
        session_name: None,
        thread_id: Some("thread-1".to_string()),
        thread_name: Some("focused work".to_string()),
        runtime_state: Some("idle".to_string()),
        rotation_state: Some("quota aware · switch stable".to_string()),
    });
    assert_eq!(
        (
            footer.snapshot().official_status.clone(),
            footer.snapshot().primary_account_email.as_deref(),
            footer.snapshot().primary_account_plan.as_deref(),
        ),
        (
            vec![FooterStatusValue::new("Status", "ready").unwrap()],
            Some("user@example.test"),
            Some("Plus"),
        )
    );
    let area = Rect::new(0, 0, 72, footer.desired_height(72));
    let mut buffer = Buffer::empty(area);
    footer.render(area, &mut buffer);
    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!("visible_runtime_projection", rendered);
}

#[test]
fn configured_rows_preserve_lanes_order_and_source_identity() {
    let config = FooterBoxConfig {
        enabled: true,
        max_rows: 3,
        border: FooterBorderStyle::Rounded,
        layout: FooterLayoutStyle::Stacked,
        adapter_ids: vec!["does-not-exist".to_string()],
        rows: vec![
            TuiFooterRow {
                left: vec![TuiFooterVariable::Model, TuiFooterVariable::ReasoningEffort],
                right: vec![TuiFooterVariable::AccountSlot],
            },
            TuiFooterRow {
                left: vec![
                    TuiFooterVariable::DisplayHandle,
                    TuiFooterVariable::SessionIdShort,
                ],
                right: vec![TuiFooterVariable::AccountPlan],
            },
            TuiFooterRow {
                left: vec![
                    TuiFooterVariable::ThreadName,
                    TuiFooterVariable::ContextUsage,
                ],
                right: vec![TuiFooterVariable::Quota],
            },
        ],
        colors: BTreeMap::from([
            (TuiFooterVariable::Model, TuiFooterColor::Cyan),
            (TuiFooterVariable::ReasoningEffort, TuiFooterColor::Magenta),
            (TuiFooterVariable::AccountSlot, TuiFooterColor::Green),
        ]),
    };
    let snapshot = FooterSnapshot::new()
        .with_live_context(FooterLiveContext {
            model: Some("gpt-5.6-sol".to_string()),
            reasoning_effort: Some("high".to_string()),
            handle: None,
            context_usage: Some("Context 42% used".to_string()),
        })
        .with_account(
            Some("user@example.test".to_string()),
            Some("Pro".to_string()),
        )
        .with_managed_slot(
            Some("2".to_string()),
            Some("C2".to_string()),
            Some("healthy".to_string()),
            Some("Week 68%".to_string()),
        )
        .with_session_thread(
            Some("01a061dc-dc83-7f43-ae26-000000000000".to_string()),
            None,
            Some("thread-1".to_string()),
            Some("Thread title is distinct".to_string()),
        );
    let footer = FooterBox::with_snapshot(config, snapshot);

    assert_eq!(
        footer.model().rows,
        vec![
            FooterModelRow {
                row: 0,
                priority: 0,
                left: vec![
                    FooterSegment::styled("gpt-5.6-sol", FooterTextStyle::Accent),
                    FooterSegment::styled("high", FooterTextStyle::Magenta),
                ],
                right: vec![FooterSegment::styled("C2", FooterTextStyle::Success)],
            },
            FooterModelRow {
                row: 1,
                priority: 1,
                left: vec![FooterSegment::new("N/A"), FooterSegment::new("01a061dc"),],
                right: vec![FooterSegment::new("Plan Pro")],
            },
            FooterModelRow {
                row: 2,
                priority: 2,
                left: vec![
                    FooterSegment::new("Thread title is distinct"),
                    FooterSegment::new("Context 42% used"),
                ],
                right: vec![FooterSegment::new("Quota Week 68%")],
            },
        ]
    );

    let area = Rect::new(0, 0, 72, footer.desired_height(72));
    let mut buffer = Buffer::empty(area);
    footer.render(area, &mut buffer);
    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("configured_footer_rows", rendered);
}

#[test]
fn display_handle_never_falls_back_to_thread_name() {
    let mut snapshot = FooterSnapshot::new();
    snapshot.thread_name = Some("Thread title".to_string());
    assert_eq!(
        configured_variable_text(TuiFooterVariable::DisplayHandle, &snapshot),
        "N/A"
    );

    snapshot.session_name = Some("Session name".to_string());
    assert_eq!(
        configured_variable_text(TuiFooterVariable::DisplayHandle, &snapshot),
        "Session name"
    );

    snapshot.handle = Some("Handle".to_string());
    assert_eq!(
        configured_variable_text(TuiFooterVariable::DisplayHandle, &snapshot),
        "Handle"
    );
}

#[test]
fn every_configured_variable_resolves_from_its_named_snapshot_field() {
    let snapshot = FooterSnapshot::new()
        .with_live_context(FooterLiveContext {
            model: Some("model-a".to_string()),
            reasoning_effort: Some("xhigh".to_string()),
            handle: Some("handle-a".to_string()),
            context_usage: Some("Context 12% used".to_string()),
        })
        .with_account(
            Some("account@example.test".to_string()),
            Some("Pro".to_string()),
        )
        .with_managed_slot(
            Some("2".to_string()),
            Some("C2".to_string()),
            Some("healthy".to_string()),
            Some("Week 68%".to_string()),
        )
        .with_session_thread(
            Some("12345678-aaaa-bbbb-cccc-dddddddddddd".to_string()),
            Some("session-a".to_string()),
            Some("thread-a".to_string()),
            Some("Thread A".to_string()),
        )
        .with_runtime_rotation(
            Some("active".to_string()),
            Some("quota aware · switch stable".to_string()),
        );

    let resolved = [
        TuiFooterVariable::Model,
        TuiFooterVariable::ReasoningEffort,
        TuiFooterVariable::AccountEmail,
        TuiFooterVariable::AccountPlan,
        TuiFooterVariable::AccountSlot,
        TuiFooterVariable::AccountSlotLabel,
        TuiFooterVariable::AccountSlotHealth,
        TuiFooterVariable::Quota,
        TuiFooterVariable::SessionId,
        TuiFooterVariable::SessionIdShort,
        TuiFooterVariable::SessionName,
        TuiFooterVariable::Handle,
        TuiFooterVariable::ThreadId,
        TuiFooterVariable::ThreadName,
        TuiFooterVariable::DisplayHandle,
        TuiFooterVariable::RuntimeState,
        TuiFooterVariable::RotationState,
        TuiFooterVariable::ContextUsage,
    ]
    .map(|variable| configured_variable_text(variable, &snapshot));

    assert_eq!(
        resolved,
        [
            "model-a",
            "xhigh",
            "Account account@example.test",
            "Plan Pro",
            "C2",
            "Slot 2",
            "healthy",
            "Quota Week 68%",
            "12345678-aaaa-bbbb-cccc-dddddddddddd",
            "12345678",
            "session-a",
            "handle-a",
            "thread-a",
            "Thread A",
            "handle-a",
            "active",
            "quota aware · switch stable",
            "Context 12% used",
        ]
    );
}

#[test]
fn configured_palette_maps_to_finite_terminal_styles() {
    assert_eq!(
        [
            TuiFooterColor::Plain,
            TuiFooterColor::Dim,
            TuiFooterColor::Red,
            TuiFooterColor::Green,
            TuiFooterColor::Yellow,
            TuiFooterColor::Blue,
            TuiFooterColor::Magenta,
            TuiFooterColor::Cyan,
            TuiFooterColor::White,
            TuiFooterColor::Gray,
        ]
        .map(FooterTextStyle::from),
        [
            FooterTextStyle::Plain,
            FooterTextStyle::Dim,
            FooterTextStyle::Red,
            FooterTextStyle::Success,
            FooterTextStyle::Warning,
            FooterTextStyle::Blue,
            FooterTextStyle::Magenta,
            FooterTextStyle::Accent,
            FooterTextStyle::White,
            FooterTextStyle::Gray,
        ]
    );
}

#[test]
fn adapter_rebuilds_only_for_adapter_rows_or_colors() {
    let rows = vec![TuiFooterRow {
        left: vec![TuiFooterVariable::Model],
        right: Vec::new(),
    }];
    let config = FooterBoxConfig {
        enabled: true,
        rows,
        ..FooterBoxConfig::default()
    };
    let mut footer = FooterBox::new(config.clone());
    let original = Arc::clone(&footer.adapters[0]);

    footer.set_config(FooterBoxConfig {
        max_rows: 3,
        border: FooterBorderStyle::Double,
        layout: FooterLayoutStyle::Compact,
        ..config.clone()
    });
    assert!(Arc::ptr_eq(&original, &footer.adapters[0]));

    footer.set_config(FooterBoxConfig {
        colors: BTreeMap::from([(TuiFooterVariable::Model, TuiFooterColor::Cyan)]),
        ..config
    });
    assert!(!Arc::ptr_eq(&original, &footer.adapters[0]));
}

#[test]
fn thread_reset_clears_live_and_runtime_values_but_keeps_account() {
    let mut footer = FooterBox::with_snapshot(
        FooterBoxConfig {
            rows: vec![TuiFooterRow {
                left: vec![TuiFooterVariable::ThreadName],
                right: Vec::new(),
            }],
            ..FooterBoxConfig::default()
        },
        FooterSnapshot::new()
            .with_account(
                Some("user@example.test".to_string()),
                Some("Pro".to_string()),
            )
            .with_live_context(FooterLiveContext {
                model: Some("old-model".to_string()),
                reasoning_effort: Some("high".to_string()),
                handle: Some("old-handle".to_string()),
                context_usage: Some("Context 99% used".to_string()),
            })
            .with_session_thread(
                Some("old-session".to_string()),
                Some("old-session-name".to_string()),
                Some("old-thread".to_string()),
                Some("old-thread-name".to_string()),
            )
            .with_managed_slot(
                Some("1".to_string()),
                Some("C1".to_string()),
                Some("healthy".to_string()),
                Some("Week 50%".to_string()),
            )
            .with_runtime_rotation(
                Some("active".to_string()),
                Some("quota aware · switch stable".to_string()),
            ),
    );

    footer.reset_thread_context();

    assert_eq!(
        footer.snapshot(),
        &FooterSnapshot::new().with_account(
            Some("user@example.test".to_string()),
            Some("Pro".to_string()),
        )
    );
}

#[test]
fn thread_reset_does_not_change_legacy_adapter_snapshot() {
    let snapshot = FooterSnapshot::new()
        .with_account(
            Some("user@example.test".to_string()),
            Some("Pro".to_string()),
        )
        .with_live_context(FooterLiveContext {
            model: Some("old-model".to_string()),
            reasoning_effort: Some("high".to_string()),
            handle: Some("old-handle".to_string()),
            context_usage: Some("Context 99% used".to_string()),
        })
        .with_runtime_projection(FooterRuntimeProjection {
            managed_slot_label: Some("1".to_string()),
            managed_slot_id: Some("C1".to_string()),
            managed_slot_health: Some("healthy".to_string()),
            managed_slot_quota: Some("Week 50%".to_string()),
            session_id: Some("old-session".to_string()),
            session_name: Some("old-session-name".to_string()),
            thread_id: Some("old-thread".to_string()),
            thread_name: Some("old-thread-name".to_string()),
            runtime_state: Some("active".to_string()),
            rotation_state: Some("quota aware · switch stable".to_string()),
            ..FooterRuntimeProjection::default()
        });
    let mut footer = FooterBox::with_snapshot(FooterBoxConfig::default(), snapshot.clone());

    footer.reset_thread_context();

    assert_eq!(footer.snapshot(), &snapshot);
}
