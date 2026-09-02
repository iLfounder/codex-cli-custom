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
