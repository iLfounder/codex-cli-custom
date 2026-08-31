//! Terminal-specific row caps for resize reflow.
//!
//! The auto cap mirrors documented scrollback defaults for terminals we can identify. Console Host
//! does not expose its configured screen buffer through terminal metadata, so it usually lands in
//! the fallback bucket.
//!
//! These caps are deliberately conservative: Codex is rebuilding normal terminal scrollback, not an
//! internal virtual transcript. Replaying more rows than the terminal retains wastes work and can
//! make interactive resize feel worse without giving the user more usable history.

use codex_config::types::DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS;
use codex_terminal_detection::Multiplexer;
use codex_terminal_detection::TerminalInfo;
use codex_terminal_detection::TerminalName;
use codex_terminal_detection::terminal_info;

use crate::legacy_core::config::TerminalResizeReflowConfig;
use crate::legacy_core::config::TerminalResizeReflowMaxRows;

const VSCODE_RESIZE_REFLOW_MAX_ROWS: usize = 1_000;
const WINDOWS_TERMINAL_RESIZE_REFLOW_MAX_ROWS: usize = 9_001;
const WEZTERM_RESIZE_REFLOW_MAX_ROWS: usize = 3_500;
const ALACRITTY_RESIZE_REFLOW_MAX_ROWS: usize = 10_000;

/// Resolve the configured row cap for resize and initial replay.
///
/// `Auto` uses terminal detection plus the VS Code environment probe because VS Code can run shells
/// whose terminal-name metadata points at the host shell rather than VS Code itself. Returning
/// `None` means the user explicitly disabled row limiting with `max_rows = 0`.
pub(crate) fn resize_reflow_max_rows(config: TerminalResizeReflowConfig) -> Option<usize> {
    resize_reflow_max_rows_for(
        config,
        &terminal_info(),
        crate::tui::running_in_vscode_terminal(),
        std::env::var_os("WT_SESSION").is_some(),
    )
}

fn resize_reflow_max_rows_for(
    config: TerminalResizeReflowConfig,
    terminal: &TerminalInfo,
    running_in_vscode_terminal: bool,
    wt_session_present: bool,
) -> Option<usize> {
    match config.max_rows {
        TerminalResizeReflowMaxRows::Auto => Some(auto_resize_reflow_max_rows(
            terminal,
            running_in_vscode_terminal,
            wt_session_present,
        )),
        TerminalResizeReflowMaxRows::Disabled => None,
        TerminalResizeReflowMaxRows::Limit(max_rows) => Some(max_rows),
    }
}

fn auto_resize_reflow_max_rows(
    terminal: &TerminalInfo,
    running_in_vscode_terminal: bool,
    wt_session_present: bool,
) -> usize {
    if running_in_vscode_terminal {
        return VSCODE_RESIZE_REFLOW_MAX_ROWS;
    }
    if (terminal.name == TerminalName::WindowsTerminal || wt_session_present) && is_tmux(terminal) {
        return DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS;
    }

    match terminal.name {
        TerminalName::VsCode => VSCODE_RESIZE_REFLOW_MAX_ROWS,
        TerminalName::WindowsTerminal => WINDOWS_TERMINAL_RESIZE_REFLOW_MAX_ROWS,
        TerminalName::WezTerm => WEZTERM_RESIZE_REFLOW_MAX_ROWS,
        TerminalName::Alacritty => ALACRITTY_RESIZE_REFLOW_MAX_ROWS,
        TerminalName::AppleTerminal
        | TerminalName::Ghostty
        | TerminalName::Iterm2
        | TerminalName::WarpTerminal
        | TerminalName::Kitty
        | TerminalName::Konsole
        | TerminalName::GnomeTerminal
        | TerminalName::Vte
        | TerminalName::Dumb
        | TerminalName::Unknown => DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS,
    }
}

fn is_tmux(terminal: &TerminalInfo) -> bool {
    matches!(
        terminal.multiplexer.as_ref(),
        Some(Multiplexer::Tmux { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_terminal(name: TerminalName) -> TerminalInfo {
        TerminalInfo {
            name,
            term_program: None,
            version: None,
            term: None,
            multiplexer: None,
        }
    }

    #[test]
    fn auto_resize_reflow_max_rows_uses_terminal_defaults() {
        let cases = [
            (TerminalName::VsCode, VSCODE_RESIZE_REFLOW_MAX_ROWS),
            (
                TerminalName::WindowsTerminal,
                WINDOWS_TERMINAL_RESIZE_REFLOW_MAX_ROWS,
            ),
            (TerminalName::WezTerm, WEZTERM_RESIZE_REFLOW_MAX_ROWS),
            (TerminalName::Alacritty, ALACRITTY_RESIZE_REFLOW_MAX_ROWS),
            (
                TerminalName::Ghostty,
                DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS,
            ),
            (
                TerminalName::Unknown,
                DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS,
            ),
        ];

        for (terminal_name, expected_max_rows) in cases {
            let terminal = test_terminal(terminal_name);
            assert_eq!(
                auto_resize_reflow_max_rows(
                    &terminal, /*running_in_vscode_terminal*/ false,
                    /*wt_session_present*/ false,
                ),
                expected_max_rows
            );
        }
    }

    #[test]
    fn auto_resize_reflow_max_rows_prefers_vscode_probe() {
        assert_eq!(
            auto_resize_reflow_max_rows(
                &test_terminal(TerminalName::WindowsTerminal),
                /*running_in_vscode_terminal*/ true,
                /*wt_session_present*/ false,
            ),
            VSCODE_RESIZE_REFLOW_MAX_ROWS
        );
    }

    #[test]
    fn configured_resize_reflow_max_rows_overrides_auto_detection() {
        let terminal = test_terminal(TerminalName::VsCode);
        let config = TerminalResizeReflowConfig {
            max_rows: TerminalResizeReflowMaxRows::Limit(42),
        };

        assert_eq!(
            resize_reflow_max_rows_for(
                config, &terminal, /*running_in_vscode_terminal*/ false,
                /*wt_session_present*/ false,
            ),
            Some(42)
        );
    }

    #[test]
    fn disabled_resize_reflow_max_rows_keeps_all_rows() {
        let terminal = test_terminal(TerminalName::VsCode);
        let config = TerminalResizeReflowConfig {
            max_rows: TerminalResizeReflowMaxRows::Disabled,
        };

        assert_eq!(
            resize_reflow_max_rows_for(
                config, &terminal, /*running_in_vscode_terminal*/ false,
                /*wt_session_present*/ false,
            ),
            None
        );
    }

    #[test]
    fn unknown_terminal_uses_fallback_even_under_multiplexer() {
        let terminal = TerminalInfo {
            name: TerminalName::Unknown,
            term_program: None,
            version: None,
            term: Some("xterm-256color".to_string()),
            multiplexer: Some(Multiplexer::Tmux { version: None }),
        };
        let config = TerminalResizeReflowConfig::default();

        assert_eq!(
            resize_reflow_max_rows_for(
                config, &terminal, /*running_in_vscode_terminal*/ false,
                /*wt_session_present*/ false,
            ),
            Some(DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS)
        );
    }

    #[test]
    fn windows_terminal_tmux_uses_fallback_without_changing_other_terminal_policies() {
        let cases = [
            (
                TerminalName::WindowsTerminal,
                None,
                false,
                WINDOWS_TERMINAL_RESIZE_REFLOW_MAX_ROWS,
            ),
            (
                TerminalName::WindowsTerminal,
                Some(Multiplexer::Tmux { version: None }),
                false,
                DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS,
            ),
            (
                TerminalName::Unknown,
                Some(Multiplexer::Tmux { version: None }),
                true,
                DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS,
            ),
            (
                TerminalName::AppleTerminal,
                Some(Multiplexer::Tmux { version: None }),
                false,
                DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS,
            ),
            (
                TerminalName::WezTerm,
                Some(Multiplexer::Tmux { version: None }),
                false,
                WEZTERM_RESIZE_REFLOW_MAX_ROWS,
            ),
            (
                TerminalName::WindowsTerminal,
                Some(Multiplexer::Zellij { version: None }),
                true,
                WINDOWS_TERMINAL_RESIZE_REFLOW_MAX_ROWS,
            ),
        ];

        for (name, multiplexer, wt_session_present, expected_max_rows) in cases {
            let mut terminal = test_terminal(name);
            terminal.multiplexer = multiplexer;
            assert_eq!(
                resize_reflow_max_rows_for(
                    TerminalResizeReflowConfig::default(),
                    &terminal,
                    /*running_in_vscode_terminal*/ false,
                    wt_session_present,
                ),
                Some(expected_max_rows)
            );
        }
    }
}
