//! Thread-scoped plugin command catalog and response presentation.

use super::*;
use crate::bottom_pane::slash_commands::PluginSlashCommand;
use crate::history_cell::PluginCommandResultHistoryCell;

impl ChatWidget {
    pub(crate) fn set_plugin_commands(&mut self, commands: Vec<PluginSlashCommand>) {
        self.bottom_pane.set_plugin_commands(commands);
    }

    pub(crate) fn submit_plugin_prompt(&mut self, prompt: String) {
        self.submit_user_message(prompt.into());
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
