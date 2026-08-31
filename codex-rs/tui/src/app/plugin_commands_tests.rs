use codex_app_server_protocol::PluginCommandTarget;
use pretty_assertions::assert_eq;

use super::*;
use crate::bottom_pane::slash_commands::BuiltinCommandFlags;
use crate::bottom_pane::slash_commands::SlashCommandItem;
use crate::bottom_pane::slash_commands::find_slash_command;

#[test]
fn projection_removes_one_backend_slash_for_render_and_dispatch() {
    let projected = project_commands(vec![PluginCommand {
        id: "fixture:review".to_string(),
        plugin_id: "fixture".to_string(),
        canonical_name: "/fixture:review".to_string(),
        short_name: Some("/fixture-review".to_string()),
        description: "Review the current change".to_string(),
        target: PluginCommandTarget::Prompt,
        available: true,
        deny_reason: None,
    }]);
    let expected = vec![
        PluginSlashCommand {
            id: "fixture:review".to_string(),
            name: "fixture:review".to_string(),
            description: "Review the current change".to_string(),
            available: true,
            deny_reason: None,
            canonical: true,
        },
        PluginSlashCommand {
            id: "fixture:review".to_string(),
            name: "fixture-review".to_string(),
            description: "Review the current change".to_string(),
            available: true,
            deny_reason: None,
            canonical: false,
        },
    ];

    assert_eq!(projected, expected);
    assert_eq!(
        find_slash_command(
            "fixture:review",
            BuiltinCommandFlags::default(),
            &[],
            &projected,
        ),
        Some(SlashCommandItem::Plugin(expected[0].clone()))
    );
    assert_eq!(
        find_slash_command(
            "fixture-review",
            BuiltinCommandFlags::default(),
            &[],
            &projected,
        ),
        Some(SlashCommandItem::Plugin(expected[1].clone()))
    );
    assert_eq!(
        projected
            .iter()
            .map(|command| format!("/{}", command.name))
            .collect::<Vec<_>>(),
        vec!["/fixture:review".to_string(), "/fixture-review".to_string(),]
    );
}
