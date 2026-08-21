//! Ephemeral plugin command and presentation cells.

use super::*;
use codex_app_server_protocol::ThreadPresentation;
use codex_app_server_protocol::ThreadPresentationNoticeLevel;

const MAX_PRESENTATION_CHARS: usize = 4_000;
const MAX_TITLE_CHARS: usize = 120;

fn bounded(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut result = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        result.push_str("…");
    }
    result
}

fn wrapped_lines(value: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.saturating_sub(4)).max(1);
    textwrap::wrap(&bounded(value, MAX_PRESENTATION_CHARS), width)
        .into_iter()
        .map(|line| line.into_owned())
        .collect()
}

#[derive(Debug)]
pub(crate) struct ThreadPresentationHistoryCell {
    item: ThreadPresentation,
}

impl ThreadPresentationHistoryCell {
    pub(crate) fn new(item: ThreadPresentation) -> Self {
        Self { item }
    }

    pub(crate) fn id(&self) -> &str {
        match &self.item {
            ThreadPresentation::Card { id, .. }
            | ThreadPresentation::Notice { id, .. }
            | ThreadPresentation::Progress { id, .. } => id,
        }
    }

    fn lines(&self, width: u16, rich: bool) -> Vec<Line<'static>> {
        match &self.item {
            ThreadPresentation::Card { title, body, .. } => {
                let title = bounded(title, MAX_TITLE_CHARS);
                let mut lines = vec![if rich {
                    vec!["◆ ".cyan(), title.bold().cyan()].into()
                } else {
                    format!("Card: {title}").into()
                }];
                lines.extend(wrapped_lines(body, width).into_iter().map(|line| {
                    if rich {
                        Line::from(format!("  {line}")).dim()
                    } else {
                        Line::from(line)
                    }
                }));
                lines
            }
            ThreadPresentation::Notice { level, message, .. } => {
                let (label, marker) = match level {
                    ThreadPresentationNoticeLevel::Info => ("Info", "●".cyan()),
                    ThreadPresentationNoticeLevel::Success => ("Success", "●".green()),
                    ThreadPresentationNoticeLevel::Warning => ("Warning", "▲".yellow()),
                    ThreadPresentationNoticeLevel::Error => ("Error", "■".red()),
                };
                wrapped_lines(message, width)
                    .into_iter()
                    .enumerate()
                    .map(|(index, line)| {
                        if rich && index == 0 {
                            vec![marker.clone(), " ".into(), line.into()].into()
                        } else if rich {
                            format!("  {line}").into()
                        } else if index == 0 {
                            format!("{label}: {line}").into()
                        } else {
                            line.into()
                        }
                    })
                    .collect()
            }
            ThreadPresentation::Progress {
                label,
                current,
                total,
                ..
            } => {
                let label = bounded(label, MAX_TITLE_CHARS);
                let progress = total.map_or_else(
                    || current.to_string(),
                    |total| format!("{current}/{total}"),
                );
                vec![if rich {
                    vec!["◒ ".magenta(), label.bold(), "  ".into(), progress.cyan()].into()
                } else {
                    format!("Progress: {label} ({progress})").into()
                }]
            }
        }
    }
}

impl HistoryCell for ThreadPresentationHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.lines(width, true)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.lines(/*width*/ 120, false)
    }
}

#[derive(Debug)]
pub(crate) struct PluginCommandResultHistoryCell {
    title: String,
    body: String,
    is_error: bool,
}

impl PluginCommandResultHistoryCell {
    pub(crate) fn new(title: String, body: String, is_error: bool) -> Self {
        Self {
            title: bounded(&title, MAX_TITLE_CHARS),
            body: bounded(&body, MAX_PRESENTATION_CHARS),
            is_error,
        }
    }
}

impl HistoryCell for PluginCommandResultHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let heading = if self.is_error {
            vec!["■ ".red(), self.title.clone().bold().red()].into()
        } else {
            vec!["◆ ".magenta(), self.title.clone().bold()].into()
        };
        std::iter::once(heading)
            .chain(
                wrapped_lines(&self.body, width)
                    .into_iter()
                    .map(|line| Line::from(format!("  {line}")).dim()),
            )
            .collect()
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![
            self.title.clone().into(),
            bounded(&self.body, MAX_PRESENTATION_CHARS).into(),
        ]
    }
}

#[cfg(test)]
#[path = "plugin_surfaces_tests.rs"]
mod tests;
