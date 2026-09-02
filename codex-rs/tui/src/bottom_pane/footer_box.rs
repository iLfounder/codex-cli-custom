//! Semantic, opt-in rendering for the contextual footer.
//!
//! The legacy [`super::footer`] module remains the compatibility renderer for transient
//! instructional states.  This module deliberately sits beside it: adapters contribute semantic
//! values, while [`FooterBox`] owns the one deterministic measure/layout/render pass used by the
//! composer.  No adapter receives terminal coordinates and no adapter performs I/O during a
//! render.

use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::render::renderable::Renderable;
use codex_config::types::TuiFooter;
use codex_config::types::TuiFooterBorder;
use codex_config::types::TuiFooterLayout;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use std::collections::BTreeMap;
use std::sync::Arc;

use super::footer_projection::FooterRuntimeProjection;

/// Adapter IDs understood by the footer registry.
pub(crate) const OFFICIAL_STATUSLINE_ADAPTER_ID: &str = "official-statusline";
pub(crate) const ACCOUNT_ADAPTER_ID: &str = "account";
pub(crate) const MANAGED_SLOT_ADAPTER_ID: &str = "managed-slot";
pub(crate) const SESSION_ADAPTER_ID: &str = "session";
pub(crate) const QUOTA_ADAPTER_ID: &str = "quota";
pub(crate) const THREAD_ADAPTER_ID: &str = "thread";
pub(crate) const RUNTIME_ADAPTER_ID: &str = "runtime";
pub(crate) const ROTATION_ADAPTER_ID: &str = "rotation";
pub(crate) const DEBUG_ADAPTER_ID: &str = "debug";

const DEFAULT_ADAPTER_IDS: [&str; 8] = [
    OFFICIAL_STATUSLINE_ADAPTER_ID,
    ACCOUNT_ADAPTER_ID,
    MANAGED_SLOT_ADAPTER_ID,
    SESSION_ADAPTER_ID,
    QUOTA_ADAPTER_ID,
    THREAD_ADAPTER_ID,
    RUNTIME_ADAPTER_ID,
    ROTATION_ADAPTER_ID,
];

const MAX_SNAPSHOT_TEXT_CHARS: usize = 192;

/// A sanitized key/value supplied to the official statusline adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FooterStatusValue {
    pub(crate) label: String,
    pub(crate) value: String,
}

impl FooterStatusValue {
    pub(crate) fn new(label: impl AsRef<str>, value: impl AsRef<str>) -> Option<Self> {
        Some(Self {
            label: sanitize_text(label.as_ref(), MAX_SNAPSHOT_TEXT_CHARS)?,
            value: sanitize_text(value.as_ref(), MAX_SNAPSHOT_TEXT_CHARS)?,
        })
    }

    fn sanitized(self) -> Option<Self> {
        Self::new(self.label, self.value)
    }
}

/// Immutable, protocol-agnostic values consumed by footer adapters.
///
/// Account and managed-slot fields intentionally contain only display-safe strings.  In
/// particular, this model has no access token, credential, or app-server protocol field.  Callers
/// replace the whole snapshot on [`FooterBox`] when an event updates the visible state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FooterSnapshot {
    /// Values exposed by the existing `/statusline` implementation.
    pub(crate) official_status: Vec<FooterStatusValue>,

    /// Primary account display fields.  These may contain a user-facing email and plan name.
    pub(crate) primary_account_email: Option<String>,
    pub(crate) primary_account_plan: Option<String>,

    /// Opaque managed-slot display fields.  The caller should provide an opaque label or ID;
    /// managed-slot email addresses are intentionally not represented here.
    pub(crate) managed_slot_label: Option<String>,
    pub(crate) managed_slot_id: Option<String>,
    pub(crate) managed_slot_health: Option<String>,
    pub(crate) managed_slot_quota: Option<String>,

    /// Generic session/thread identifiers and labels.
    pub(crate) session_id: Option<String>,
    pub(crate) session_name: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) thread_name: Option<String>,

    /// Generic runtime and rotation state labels supplied by the TUI state owner.
    pub(crate) runtime_state: Option<String>,
    pub(crate) rotation_state: Option<String>,

    /// Optional bounded text reserved for the explicit debug adapter.
    pub(crate) debug: Option<String>,
}

impl FooterSnapshot {
    /// Return an empty snapshot.  Empty snapshots produce no semantic footer rows.
    pub(crate) const fn new() -> Self {
        Self {
            official_status: Vec::new(),
            primary_account_email: None,
            primary_account_plan: None,
            managed_slot_label: None,
            managed_slot_id: None,
            managed_slot_health: None,
            managed_slot_quota: None,
            session_id: None,
            session_name: None,
            thread_id: None,
            thread_name: None,
            runtime_state: None,
            rotation_state: None,
            debug: None,
        }
    }

    /// Build a snapshot containing one display-safe value from an existing status line.
    pub(crate) fn from_status_line(line: &Line<'_>) -> Self {
        Self::new().with_status_line(Some(line))
    }

    /// Replace the official statusline projection while retaining the rest of the snapshot.
    ///
    /// The status line is owned by the legacy footer state machine.  Keeping this small adapter
    /// here lets that state machine update the immutable semantic snapshot without exposing any
    /// protocol/account types to the footer renderer.
    pub(crate) fn with_status_line(mut self, line: Option<&Line<'_>>) -> Self {
        self.official_status = line
            .and_then(|line| {
                let text = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                FooterStatusValue::new("Status", text)
            })
            .into_iter()
            .collect();
        self
    }

    /// Replace official statusline values, sanitizing each value and dropping empty entries.
    pub(crate) fn with_official_status(mut self, values: Vec<FooterStatusValue>) -> Self {
        self.official_status = values
            .into_iter()
            .filter_map(FooterStatusValue::sanitized)
            .collect();
        self
    }

    /// Set the primary account's display-safe email and plan.
    pub(crate) fn with_account(mut self, email: Option<String>, plan: Option<String>) -> Self {
        self.primary_account_email = sanitize_optional(email);
        self.primary_account_plan = sanitize_optional(plan);
        self
    }

    /// Set opaque managed-slot display fields.  No credential or email field is accepted.
    pub(crate) fn with_managed_slot(
        mut self,
        label: Option<String>,
        id: Option<String>,
        health: Option<String>,
        quota: Option<String>,
    ) -> Self {
        self.managed_slot_label = sanitize_optional(label);
        self.managed_slot_id = sanitize_optional(id);
        self.managed_slot_health = sanitize_optional(health);
        self.managed_slot_quota = sanitize_optional(quota);
        self
    }

    /// Set generic session and thread display fields.
    pub(crate) fn with_session_thread(
        mut self,
        session_id: Option<String>,
        session_name: Option<String>,
        thread_id: Option<String>,
        thread_name: Option<String>,
    ) -> Self {
        self.session_id = sanitize_optional(session_id);
        self.session_name = sanitize_optional(session_name);
        self.thread_id = sanitize_optional(thread_id);
        self.thread_name = sanitize_optional(thread_name);
        self
    }

    /// Set generic runtime and rotation labels.
    pub(crate) fn with_runtime_rotation(
        mut self,
        runtime_state: Option<String>,
        rotation_state: Option<String>,
    ) -> Self {
        self.runtime_state = sanitize_optional(runtime_state);
        self.rotation_state = sanitize_optional(rotation_state);
        self
    }

    /// Replace runtime-owned fields while retaining primary account and official status values.
    pub(crate) fn with_runtime_projection(mut self, projection: FooterRuntimeProjection) -> Self {
        self.managed_slot_label = projection.managed_slot_label;
        self.managed_slot_id = projection.managed_slot_id;
        self.managed_slot_health = projection.managed_slot_health;
        self.managed_slot_quota = projection.managed_slot_quota;
        self.session_id = projection.session_id;
        self.session_name = projection.session_name;
        self.thread_id = projection.thread_id;
        self.thread_name = projection.thread_name;
        self.runtime_state = projection.runtime_state;
        self.rotation_state = projection.rotation_state;
        self.sanitized()
    }

    /// Set bounded debug text for the opt-in debug adapter.
    pub(crate) fn with_debug(mut self, debug: Option<String>) -> Self {
        self.debug = sanitize_optional(debug);
        self
    }

    /// Return a sanitized copy, useful when a snapshot was assembled with struct fields directly.
    pub(crate) fn sanitized(self) -> Self {
        Self {
            official_status: self
                .official_status
                .into_iter()
                .filter_map(FooterStatusValue::sanitized)
                .collect(),
            primary_account_email: sanitize_optional(self.primary_account_email),
            primary_account_plan: sanitize_optional(self.primary_account_plan),
            managed_slot_label: sanitize_optional(self.managed_slot_label),
            managed_slot_id: sanitize_optional(self.managed_slot_id),
            managed_slot_health: sanitize_optional(self.managed_slot_health),
            managed_slot_quota: sanitize_optional(self.managed_slot_quota),
            session_id: sanitize_optional(self.session_id),
            session_name: sanitize_optional(self.session_name),
            thread_id: sanitize_optional(self.thread_id),
            thread_name: sanitize_optional(self.thread_name),
            runtime_state: sanitize_optional(self.runtime_state),
            rotation_state: sanitize_optional(self.rotation_state),
            debug: sanitize_optional(self.debug),
        }
    }
}

fn sanitize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| sanitize_text(&value, MAX_SNAPSHOT_TEXT_CHARS))
}

fn sanitize_text(value: &str, max_chars: usize) -> Option<String> {
    let mut normalized = String::with_capacity(value.len().min(max_chars));
    let mut previous_was_space = false;
    for ch in value.chars() {
        if ch.is_control() || ch.is_whitespace() {
            if !previous_was_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            previous_was_space = true;
        } else {
            normalized.push(ch);
            previous_was_space = false;
        }
    }
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return None;
    }
    let mut chars = normalized.chars();
    let mut bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    Some(bounded)
}

/// Which horizontal lane receives a semantic contribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FooterLane {
    Left,
    Right,
}

/// Style intent carried by a semantic segment.  Adapters do not construct ratatui coordinates or
/// pre-truncated spans; the box maps this intent to terminal styles during rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FooterTextStyle {
    Plain,
    Dim,
    Accent,
    Success,
    Warning,
}

/// A semantic piece of footer text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FooterSegment {
    pub(crate) text: String,
    pub(crate) style: FooterTextStyle,
}

impl FooterSegment {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: FooterTextStyle::Plain,
        }
    }

    pub(crate) fn styled(text: impl Into<String>, style: FooterTextStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    pub(crate) fn labeled(label: impl AsRef<str>, value: impl AsRef<str>) -> Option<Self> {
        let label = sanitize_text(label.as_ref(), MAX_SNAPSHOT_TEXT_CHARS)?;
        let value = sanitize_text(value.as_ref(), MAX_SNAPSHOT_TEXT_CHARS)?;
        Some(Self::new(format!("{label}: {value}")))
    }
}

/// Semantic adapter output.  `row` is a preferred ordering key, not a terminal coordinate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FooterContribution {
    pub(crate) row: usize,
    pub(crate) lane: FooterLane,
    pub(crate) priority: u8,
    pub(crate) segments: Vec<FooterSegment>,
}

impl FooterContribution {
    pub(crate) fn new(
        row: usize,
        lane: FooterLane,
        priority: u8,
        segments: Vec<FooterSegment>,
    ) -> Self {
        Self {
            row,
            lane,
            priority,
            segments,
        }
    }

    pub(crate) fn single(
        row: usize,
        lane: FooterLane,
        priority: u8,
        text: impl Into<String>,
    ) -> Self {
        Self::new(row, lane, priority, vec![FooterSegment::new(text)])
    }
}

/// Pure adapter contract for semantic footer data.
///
/// Implementations should only inspect `snapshot` and append display-safe semantic
/// [`FooterContribution`] values.  They must not perform network, credential, filesystem, or
/// terminal I/O, and they must not depend on the `Rect` that will eventually render the result.
pub(crate) trait FooterAdapter: Send + Sync {
    /// Stable registry ID used by [`FooterBoxConfig::adapter_ids`].
    fn id(&self) -> &'static str;

    /// Append this adapter's semantic values to `out`.
    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FooterModelRow {
    pub(crate) row: usize,
    pub(crate) priority: u8,
    pub(crate) left: Vec<FooterSegment>,
    pub(crate) right: Vec<FooterSegment>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FooterModel {
    pub(crate) rows: Vec<FooterModelRow>,
}

impl FooterModel {
    fn selected_rows(&self, max_rows: usize) -> Vec<FooterModelRow> {
        if max_rows == 0 || self.rows.is_empty() {
            return Vec::new();
        }
        let mut selected = self.rows.clone();
        selected.sort_by_key(|row| (row.priority, row.row));
        selected.truncate(max_rows);
        selected.sort_by_key(|row| row.row);
        selected
    }
}

/// Border style used by the TUI renderer after converting config values into local presentation
/// types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FooterBorderStyle {
    None,
    Plain,
    Rounded,
    Double,
}

impl Default for FooterBorderStyle {
    fn default() -> Self {
        Self::None
    }
}

impl From<TuiFooterBorder> for FooterBorderStyle {
    fn from(value: TuiFooterBorder) -> Self {
        match value {
            TuiFooterBorder::None => Self::None,
            TuiFooterBorder::Plain => Self::Plain,
            TuiFooterBorder::Rounded => Self::Rounded,
            TuiFooterBorder::Double => Self::Double,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FooterLayoutStyle {
    Stacked,
    Compact,
}

impl Default for FooterLayoutStyle {
    fn default() -> Self {
        Self::Stacked
    }
}

impl From<TuiFooterLayout> for FooterLayoutStyle {
    fn from(value: TuiFooterLayout) -> Self {
        match value {
            TuiFooterLayout::Stacked => Self::Stacked,
            TuiFooterLayout::Compact => Self::Compact,
        }
    }
}

/// Runtime presentation config for [`FooterBox`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FooterBoxConfig {
    pub(crate) enabled: bool,
    pub(crate) max_rows: u16,
    pub(crate) border: FooterBorderStyle,
    pub(crate) layout: FooterLayoutStyle,
    pub(crate) adapter_ids: Vec<String>,
}

impl Default for FooterBoxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_rows: 1,
            border: FooterBorderStyle::None,
            layout: FooterLayoutStyle::Stacked,
            adapter_ids: Vec::new(),
        }
    }
}

impl From<&TuiFooter> for FooterBoxConfig {
    fn from(value: &TuiFooter) -> Self {
        Self {
            enabled: value.enabled,
            max_rows: value.max_rows.max(1),
            border: value.border.into(),
            layout: value.layout.into(),
            adapter_ids: value.adapter_ids.clone(),
        }
    }
}

impl FooterBoxConfig {
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub(crate) fn with_adapter_ids<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.adapter_ids = ids.into_iter().map(Into::into).collect();
        self
    }
}

/// A measured footer box.  The dimensions are computed from the same semantic model used by
/// [`FooterBox::layout`] and [`FooterBox::render`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FooterBoxMeasure {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) content_width: u16,
    pub(crate) rows: u16,
    pub(crate) border: FooterBorderStyle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FooterRowLayout {
    pub(crate) area: Rect,
    pub(crate) left: Line<'static>,
    pub(crate) right: Option<Line<'static>>,
    pub(crate) right_x: Option<u16>,
}

/// Immutable layout plan consumed by `render_layout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FooterBoxLayout {
    pub(crate) area: Rect,
    pub(crate) content_area: Rect,
    pub(crate) border: FooterBorderStyle,
    pub(crate) rows: Vec<FooterRowLayout>,
}

impl FooterBoxLayout {
    fn empty(area: Rect) -> Self {
        Self {
            area,
            content_area: Rect::default(),
            border: FooterBorderStyle::None,
            rows: Vec::new(),
        }
    }
}

/// Semantic footer renderer used only for passive/contextual composer content.
pub(crate) struct FooterBox {
    config: FooterBoxConfig,
    snapshot: FooterSnapshot,
    adapters: Vec<Arc<dyn FooterAdapter>>,
}

impl std::fmt::Debug for FooterBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FooterBox")
            .field("config", &self.config)
            .field("snapshot", &self.snapshot)
            .field(
                "adapters",
                &self
                    .adapters
                    .iter()
                    .map(|adapter| adapter.id())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl FooterBox {
    pub(crate) fn new(config: FooterBoxConfig) -> Self {
        let adapters = adapters_for_ids(&config.adapter_ids);
        Self {
            config,
            snapshot: FooterSnapshot::new(),
            adapters,
        }
    }

    pub(crate) fn with_snapshot(config: FooterBoxConfig, snapshot: FooterSnapshot) -> Self {
        let mut footer = Self::new(config);
        footer.snapshot = snapshot.sanitized();
        footer
    }

    pub(crate) fn config(&self) -> &FooterBoxConfig {
        &self.config
    }

    pub(crate) fn snapshot(&self) -> &FooterSnapshot {
        &self.snapshot
    }

    pub(crate) fn set_config(&mut self, config: FooterBoxConfig) {
        if self.config.adapter_ids != config.adapter_ids {
            self.adapters = adapters_for_ids(&config.adapter_ids);
        }
        self.config = config;
    }

    pub(crate) fn set_snapshot(&mut self, snapshot: FooterSnapshot) {
        self.snapshot = snapshot.sanitized();
    }

    /// Synchronize runtime-owned fields without replacing account or official status values.
    pub(crate) fn set_runtime_projection(&mut self, projection: FooterRuntimeProjection) {
        self.snapshot = self.snapshot.clone().with_runtime_projection(projection);
    }

    /// Synchronize the official statusline projection without replacing account/session data.
    pub(crate) fn set_status_line(&mut self, line: Option<&Line<'_>>) {
        self.set_snapshot(self.snapshot.clone().with_status_line(line));
    }

    /// Synchronize display-safe account fields without exposing credential/runtime state.
    pub(crate) fn set_account(&mut self, email: Option<String>, plan: Option<String>) {
        self.set_snapshot(self.snapshot.clone().with_account(email, plan));
    }

    pub(crate) fn set_adapters(&mut self, adapters: Vec<Arc<dyn FooterAdapter>>) {
        self.adapters = adapters;
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Build the immutable semantic model for the current snapshot.
    pub(crate) fn model(&self) -> FooterModel {
        if !self.config.enabled {
            return FooterModel::default();
        }

        let mut contributions = Vec::new();
        for adapter in &self.adapters {
            adapter.contribute(&self.snapshot, &mut contributions);
        }

        let mut rows = BTreeMap::<usize, FooterModelRow>::new();
        for contribution in contributions {
            let segments = contribution
                .segments
                .into_iter()
                .filter(|segment| !segment.text.is_empty())
                .collect::<Vec<_>>();
            if segments.is_empty() {
                continue;
            }
            let row = rows
                .entry(contribution.row)
                .or_insert_with(|| FooterModelRow {
                    row: contribution.row,
                    priority: contribution.priority,
                    left: Vec::new(),
                    right: Vec::new(),
                });
            row.priority = row.priority.min(contribution.priority);
            match contribution.lane {
                FooterLane::Left => row.left.extend(segments),
                FooterLane::Right => row.right.extend(segments),
            }
        }

        let mut rows = rows.into_values().collect::<Vec<_>>();
        if self.config.layout == FooterLayoutStyle::Compact && rows.len() > 1 {
            let mut compact = FooterModelRow {
                row: 0,
                priority: rows.iter().map(|row| row.priority).min().unwrap_or(0),
                left: Vec::new(),
                right: Vec::new(),
            };
            for row in rows {
                compact.left.extend(row.left);
                compact.right.extend(row.right);
            }
            rows = vec![compact];
        }
        FooterModel { rows }
    }

    /// Measure the preferred height for a terminal width.
    pub(crate) fn measure(&self, width: u16) -> FooterBoxMeasure {
        let model = self.model();
        if !self.config.enabled || width == 0 || model.rows.is_empty() {
            return FooterBoxMeasure {
                width,
                ..FooterBoxMeasure::default()
            };
        }

        let (border, padding) = chrome_for_width(self.config.border, width);
        let rows = model
            .selected_rows(usize::from(self.config.max_rows.max(1)))
            .len() as u16;
        let chrome_width = border_width(border) + padding.saturating_mul(2);
        let content_width = width.saturating_sub(chrome_width);
        FooterBoxMeasure {
            width,
            height: rows
                .saturating_add(border_height(border))
                .saturating_add(padding.saturating_mul(2)),
            content_width,
            rows,
            border,
        }
    }

    /// Return the deterministic layout plan for `area`.
    pub(crate) fn layout(&self, area: Rect) -> FooterBoxLayout {
        let model = self.model();
        if !self.config.enabled || area.is_empty() || model.rows.is_empty() {
            return FooterBoxLayout::empty(area);
        }

        let (mut border, mut padding) = chrome_for_width(self.config.border, area.width);
        let requested_rows = usize::from(self.config.max_rows.max(1));
        let chrome_height = border_height(border).saturating_add(padding.saturating_mul(2));
        let min_height = chrome_height.saturating_add(1);
        let min_width = border_width(border).saturating_add(padding.saturating_mul(2)) + 1;

        // A bordered multi-row box must never consume the whole area without a content row.  If
        // a popup/resize leaves less room, deterministically fall back to one borderless row.
        if area.height < min_height || area.width < min_width {
            border = FooterBorderStyle::None;
            padding = 0;
        }

        let chrome_height = border_height(border).saturating_add(padding.saturating_mul(2));
        let available_rows = area.height.saturating_sub(chrome_height);
        if available_rows == 0 {
            return FooterBoxLayout::empty(area);
        }
        let row_count = requested_rows.min(usize::from(available_rows)).max(1);
        let rows = model.selected_rows(row_count);
        if rows.is_empty() {
            return FooterBoxLayout::empty(area);
        }

        let border_offset = border_thickness(border);
        let content_x = area.x.saturating_add(border_offset).saturating_add(padding);
        let content_y = area.y.saturating_add(border_offset).saturating_add(padding);
        let content_width = area
            .width
            .saturating_sub(border_width(border))
            .saturating_sub(padding.saturating_mul(2));
        let content_area = Rect::new(content_x, content_y, content_width, rows.len() as u16);

        let row_layouts = rows
            .into_iter()
            .enumerate()
            .map(|(idx, row)| fit_row(row, content_area, idx as u16))
            .collect();

        FooterBoxLayout {
            area,
            content_area,
            border,
            rows: row_layouts,
        }
    }

    /// Render using the same layout policy returned by [`Self::layout`].
    pub(crate) fn render_layout(&self, layout: &FooterBoxLayout, buf: &mut Buffer) {
        if layout.rows.is_empty() || layout.area.is_empty() {
            return;
        }
        render_border(layout.border, layout.area, buf);
        for row in &layout.rows {
            if !row.area.is_empty() {
                Paragraph::new(row.left.clone()).render(row.area, buf);
            }
            if let (Some(right), Some(right_x)) = (&row.right, row.right_x) {
                let right_area = Rect::new(right_x, row.area.y, right.width() as u16, 1);
                if !right_area.is_empty() {
                    Paragraph::new(right.clone()).render(right_area, buf);
                }
            }
        }
    }

    pub(crate) fn has_content(&self) -> bool {
        !self.model().rows.is_empty()
    }
}

impl Renderable for FooterBox {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let layout = self.layout(area);
        self.render_layout(&layout, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.measure(width).height
    }
}

fn chrome_for_width(border: FooterBorderStyle, width: u16) -> (FooterBorderStyle, u16) {
    // A bordered box reserves one cell of horizontal padding on each side and
    // one cell for each vertical border. Keep the same minimum-width policy as
    // `layout`: a bordered row needs at least one content cell (2 + 2 + 1).
    // Returning border chrome for widths 3–4 would make `measure` reserve
    // rows that `layout` must subsequently discard during its narrow-area
    // fallback, leaving blank space in the composer.
    if border != FooterBorderStyle::None && width >= 5 {
        (border, 1)
    } else {
        (FooterBorderStyle::None, 0)
    }
}

const fn border_thickness(border: FooterBorderStyle) -> u16 {
    if matches!(border, FooterBorderStyle::None) {
        0
    } else {
        1
    }
}

const fn border_width(border: FooterBorderStyle) -> u16 {
    border_thickness(border).saturating_mul(2)
}

const fn border_height(border: FooterBorderStyle) -> u16 {
    border_width(border)
}

fn fit_row(row: FooterModelRow, content_area: Rect, row_offset: u16) -> FooterRowLayout {
    let area = Rect::new(
        content_area.x,
        content_area.y.saturating_add(row_offset),
        content_area.width,
        1,
    );
    let mut left = semantic_line(&row.left);
    let mut right = semantic_line(&row.right);
    let width = area.width as usize;
    let right_width = right.width();
    let right_budget = if right_width > 0 {
        width.saturating_sub(1).min(right_width)
    } else {
        0
    };
    if right_width > right_budget {
        right = truncate_line_with_ellipsis_if_overflow(right, right_budget);
    }
    let right_width = right.width();
    let left_budget = if right_width > 0 {
        width.saturating_sub(right_width).saturating_sub(1)
    } else {
        width
    };
    if left.width() > left_budget {
        left = truncate_line_with_ellipsis_if_overflow(left, left_budget);
    }

    let right = (right.width() > 0).then_some(right);
    let right_x = right.as_ref().and_then(|line| {
        let right_width = line.width() as u16;
        (right_width <= area.width).then_some(area.right().saturating_sub(right_width))
    });
    FooterRowLayout {
        area,
        left,
        right,
        right_x,
    }
}

fn semantic_line(segments: &[FooterSegment]) -> Line<'static> {
    let mut spans = Vec::with_capacity(segments.len().saturating_mul(2));
    for (idx, segment) in segments.iter().enumerate() {
        if idx > 0 {
            spans.push(" · ".dim());
        }
        spans.push(segment_span(segment));
    }
    Line::from(spans)
}

fn segment_span(segment: &FooterSegment) -> Span<'static> {
    match segment.style {
        FooterTextStyle::Plain => Span::from(segment.text.clone()),
        FooterTextStyle::Dim => Span::from(segment.text.clone()).dim(),
        FooterTextStyle::Accent => Span::from(segment.text.clone()).cyan(),
        FooterTextStyle::Success => Span::from(segment.text.clone()).green(),
        FooterTextStyle::Warning => Span::from(segment.text.clone()).yellow(),
    }
}

fn render_border(style: FooterBorderStyle, area: Rect, buf: &mut Buffer) {
    if style == FooterBorderStyle::None {
        return;
    }
    let border_type = match style {
        FooterBorderStyle::None => BorderType::Plain,
        FooterBorderStyle::Plain => BorderType::Plain,
        FooterBorderStyle::Rounded => BorderType::Rounded,
        FooterBorderStyle::Double => BorderType::Double,
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        .render(area, buf);
}

fn adapters_for_ids(ids: &[String]) -> Vec<Arc<dyn FooterAdapter>> {
    // Keep unknown IDs out of the render path. This parser is intentionally separate from the
    // exhaustive status-line item parser so adding a footer adapter cannot change `/statusline`
    // validation. Iterate over either borrowed source directly; rebuilding a config must not leak    // an allocation for every custom adapter list.
    if ids.is_empty() {
        DEFAULT_ADAPTER_IDS
            .iter()
            .filter_map(|id| adapter_for_id(id))
            .collect()
    } else {
        ids.iter()
            .map(String::as_str)
            .filter_map(adapter_for_id)
            .collect()
    }
}

fn adapter_for_id(id: &str) -> Option<Arc<dyn FooterAdapter>> {
    match id {
        OFFICIAL_STATUSLINE_ADAPTER_ID => Some(Arc::new(OfficialStatuslineAdapter)),
        ACCOUNT_ADAPTER_ID => Some(Arc::new(AccountAdapter)),
        MANAGED_SLOT_ADAPTER_ID => Some(Arc::new(ManagedSlotAdapter)),
        SESSION_ADAPTER_ID => Some(Arc::new(SessionAdapter)),
        QUOTA_ADAPTER_ID => Some(Arc::new(QuotaAdapter)),
        THREAD_ADAPTER_ID => Some(Arc::new(ThreadAdapter)),
        RUNTIME_ADAPTER_ID => Some(Arc::new(RuntimeAdapter)),
        ROTATION_ADAPTER_ID => Some(Arc::new(RotationAdapter)),
        DEBUG_ADAPTER_ID => Some(Arc::new(DebugAdapter)),
        _ => None,
    }
}

struct OfficialStatuslineAdapter;

impl FooterAdapter for OfficialStatuslineAdapter {
    fn id(&self) -> &'static str {
        OFFICIAL_STATUSLINE_ADAPTER_ID
    }

    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>) {
        let segments = snapshot
            .official_status
            .iter()
            .filter_map(|value| FooterSegment::labeled(&value.label, &value.value))
            .map(|segment| FooterSegment::styled(segment.text, FooterTextStyle::Dim))
            .collect::<Vec<_>>();
        if !segments.is_empty() {
            out.push(FooterContribution::new(0, FooterLane::Left, 10, segments));
        }
    }
}

struct AccountAdapter;

impl FooterAdapter for AccountAdapter {
    fn id(&self) -> &'static str {
        ACCOUNT_ADAPTER_ID
    }

    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>) {
        let mut segments = Vec::new();
        if let Some(email) = snapshot.primary_account_email.as_deref() {
            segments.push(FooterSegment::styled(
                format!("Account {email}"),
                FooterTextStyle::Accent,
            ));
        }
        if let Some(plan) = snapshot.primary_account_plan.as_deref() {
            segments.push(FooterSegment::styled(
                format!("Plan {plan}"),
                FooterTextStyle::Dim,
            ));
        }
        if !segments.is_empty() {
            out.push(FooterContribution::new(1, FooterLane::Left, 20, segments));
        }
    }
}

struct ManagedSlotAdapter;

impl FooterAdapter for ManagedSlotAdapter {
    fn id(&self) -> &'static str {
        MANAGED_SLOT_ADAPTER_ID
    }

    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>) {
        let mut left = Vec::new();
        if let Some(label) = snapshot.managed_slot_label.as_deref() {
            left.push(FooterSegment::styled(
                format!("Slot {label}"),
                FooterTextStyle::Accent,
            ));
        }
        if let Some(id) = snapshot.managed_slot_id.as_deref() {
            left.push(FooterSegment::styled(
                format!("id {id}"),
                FooterTextStyle::Dim,
            ));
        }
        if !left.is_empty() {
            out.push(FooterContribution::new(2, FooterLane::Left, 30, left));
        }
        if let Some(health) = snapshot.managed_slot_health.as_deref() {
            out.push(FooterContribution::single(
                2,
                FooterLane::Right,
                30,
                health.to_string(),
            ));
        }
    }
}

struct SessionAdapter;

impl FooterAdapter for SessionAdapter {
    fn id(&self) -> &'static str {
        SESSION_ADAPTER_ID
    }

    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>) {
        let mut segments = Vec::new();
        if let Some(name) = snapshot.session_name.as_deref() {
            segments.push(FooterSegment::styled(
                format!("Session {name}"),
                FooterTextStyle::Plain,
            ));
        }
        if let Some(id) = snapshot.session_id.as_deref() {
            segments.push(FooterSegment::styled(
                format!("id {id}"),
                FooterTextStyle::Dim,
            ));
        }
        if !segments.is_empty() {
            out.push(FooterContribution::new(3, FooterLane::Left, 40, segments));
        }
    }
}

struct QuotaAdapter;

impl FooterAdapter for QuotaAdapter {
    fn id(&self) -> &'static str {
        QUOTA_ADAPTER_ID
    }

    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>) {
        if let Some(quota) = snapshot.managed_slot_quota.as_deref() {
            out.push(FooterContribution::single(
                4,
                FooterLane::Right,
                50,
                format!("quota {quota}"),
            ));
        }
    }
}

struct ThreadAdapter;

impl FooterAdapter for ThreadAdapter {
    fn id(&self) -> &'static str {
        THREAD_ADAPTER_ID
    }

    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>) {
        let mut segments = Vec::new();
        if let Some(name) = snapshot.thread_name.as_deref() {
            segments.push(FooterSegment::new(format!("Thread {name}")));
        }
        if let Some(id) = snapshot.thread_id.as_deref() {
            segments.push(FooterSegment::styled(
                format!("id {id}"),
                FooterTextStyle::Dim,
            ));
        }
        if !segments.is_empty() {
            out.push(FooterContribution::new(5, FooterLane::Left, 60, segments));
        }
    }
}

struct RuntimeAdapter;

impl FooterAdapter for RuntimeAdapter {
    fn id(&self) -> &'static str {
        RUNTIME_ADAPTER_ID
    }

    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>) {
        if let Some(state) = snapshot.runtime_state.as_deref() {
            out.push(FooterContribution::single(
                6,
                FooterLane::Left,
                70,
                format!("Runtime {state}"),
            ));
        }
    }
}

struct RotationAdapter;

impl FooterAdapter for RotationAdapter {
    fn id(&self) -> &'static str {
        ROTATION_ADAPTER_ID
    }

    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>) {
        if let Some(state) = snapshot.rotation_state.as_deref() {
            out.push(FooterContribution::single(
                7,
                FooterLane::Left,
                80,
                format!("Rotation {state}"),
            ));
        }
    }
}

struct DebugAdapter;

impl FooterAdapter for DebugAdapter {
    fn id(&self) -> &'static str {
        DEBUG_ADAPTER_ID
    }

    fn contribute(&self, snapshot: &FooterSnapshot, out: &mut Vec<FooterContribution>) {
        if let Some(debug) = snapshot.debug.as_deref() {
            out.push(FooterContribution::single(
                8,
                FooterLane::Left,
                250,
                format!("debug {debug}"),
            ));
        }
    }
}

#[cfg(test)]
#[path = "footer_box_tests.rs"]
mod tests;
