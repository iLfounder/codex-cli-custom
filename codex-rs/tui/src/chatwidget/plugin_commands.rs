//! Thread-scoped plugin command catalog and response presentation.

use super::*;
use crate::bottom_pane::slash_commands::PluginSlashCommand;
use crate::history_cell::PluginCommandResultHistoryCell;
use codex_app_server_protocol::SessionRuntimeAccountRef;

impl ChatWidget {
    pub(crate) fn set_plugin_commands(&mut self, commands: Vec<PluginSlashCommand>) {
        self.bottom_pane.set_plugin_commands(commands);
    }

    pub(crate) fn submit_plugin_prompt(
        &mut self,
        prompt: String,
        account: SessionRuntimeAccountRef,
    ) {
        let _ = self.submit_user_message_with_history_and_shell_escape_policy(
            prompt.into(),
            UserMessageHistoryRecord::PluginPrompt {
                account: Some(account),
                history: None,
            },
            ShellEscapePolicy::Disallow,
        );
    }

    pub(crate) fn add_plugin_command_result(
        &mut self,
        title: String,
        body: String,
        is_error: bool,
    ) {
        self.add_to_history(PluginCommandResultHistoryCell::new(title, body, is_error));
    }
}

#[cfg(test)]
#[path = "plugin_commands_tests.rs"]
mod tests;
