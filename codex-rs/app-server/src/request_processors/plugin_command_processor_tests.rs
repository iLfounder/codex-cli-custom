use pretty_assertions::assert_eq;

use super::*;

fn resolved(namespace: &str, name: &str) -> ResolvedCommand {
    resolved_contribution(
        namespace,
        PluginCommandContribution {
            id: name.to_string(),
            name: name.to_string(),
            description: String::new(),
            target: PluginCommandTarget::Prompt {
                prompt: "fixed".to_string(),
            },
        },
    )
}

fn resolved_contribution(
    namespace: &str,
    contribution: PluginCommandContribution,
) -> ResolvedCommand {
    match resolved_command(
        namespace,
        namespace,
        contribution,
        true,
        None,
        account_ref(),
    ) {
        Ok(command) => command,
        Err(error) => panic!(
            "failed to resolve fixture plugin command: {}",
            error.message
        ),
    }
}

fn account_ref() -> SessionRuntimeAccountRef {
    SessionRuntimeAccountRef {
        account_slot_id: "slot-1".to_string(),
        execution_generation: 7,
    }
}

#[test]
fn resolved_prompt_keeps_the_catalog_account_capture() {
    assert_eq!(resolved("alpha", "deploy").execution_account, account_ref());
}

#[test]
fn assigns_only_unambiguous_short_names_and_disables_canonical_collisions() {
    let mut commands = vec![
        resolved("alpha", "deploy"),
        resolved("beta", "deploy"),
        resolved("alpha", "ship"),
        resolved("alpha", "ship"),
    ];

    assign_resolution_names(&mut commands);

    assert_eq!(commands[0].api.short_name, None);
    assert_eq!(commands[1].api.short_name, None);
    assert!(!commands[2].api.available);
    assert!(!commands[3].api.available);
}

#[test]
fn suppresses_builtin_and_builtin_alias_short_names() {
    let mut commands = vec![
        resolved("alpha", "review"),
        resolved("alpha", "clean"),
        resolved("alpha", "goooooal"),
        resolved("alpha", "inspect"),
    ];

    assign_resolution_names(&mut commands);

    assert_eq!(
        commands
            .iter()
            .map(|command| command.api.short_name.as_deref())
            .collect::<Vec<_>>(),
        vec![None, None, None, Some("/inspect")]
    );
}

#[test]
fn command_id_changes_with_resolved_target() {
    let original = resolved("alpha", "deploy");
    let changed = resolved_contribution(
        "alpha",
        PluginCommandContribution {
            id: "deploy".to_string(),
            name: "deploy".to_string(),
            description: String::new(),
            target: PluginCommandTarget::Prompt {
                prompt: "changed".to_string(),
            },
        },
    );

    assert_ne!(original.api.id, changed.api.id);
}

#[test]
fn command_id_normalizes_mcp_argument_object_order() {
    let contribution = |arguments| PluginCommandContribution {
        id: "deploy".to_string(),
        name: "deploy".to_string(),
        description: String::new(),
        target: PluginCommandTarget::McpTool {
            server: "fixture".to_string(),
            tool: "deploy".to_string(),
            arguments: Some(arguments),
        },
    };
    let first = resolved_contribution("alpha", contribution(serde_json::json!({"b": 2, "a": 1})));
    let reordered =
        resolved_contribution("alpha", contribution(serde_json::json!({"a": 1, "b": 2})));

    assert_eq!(first.api.id, reordered.api.id);
}

#[test]
fn validates_bounded_progress() {
    assert_eq!(
        validate_presentation(&ThreadPresentation::Progress {
            id: "build".to_string(),
            label: "Compiling".to_string(),
            current: 2,
            total: Some(3),
        }),
        Ok(())
    );
    assert!(
        validate_presentation(&ThreadPresentation::Progress {
            id: "build".to_string(),
            label: "Compiling".to_string(),
            current: 4,
            total: Some(3),
        })
        .is_err()
    );
}
