use pretty_assertions::assert_eq;

use super::*;

fn resolved(namespace: &str, name: &str) -> ResolvedCommand {
    resolved_command(
        namespace,
        namespace,
        PluginCommandContribution {
            id: name.to_string(),
            name: name.to_string(),
            description: String::new(),
            target: PluginCommandTarget::Prompt {
                prompt: "fixed".to_string(),
            },
        },
        true,
        None,
    )
}

#[test]
fn assigns_only_unambiguous_short_names_and_disables_canonical_collisions() {
    let mut commands = vec![
        resolved("alpha", "review"),
        resolved("beta", "review"),
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
