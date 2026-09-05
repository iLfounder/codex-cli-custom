use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn user_layer(profile: Option<&str>, config: serde_json::Value) -> ConfigLayer {
    ConfigLayer {
        name: ConfigLayerSource::User {
            file: LegacyAppPathString::from_string("/server/codex/config.toml"),
            profile: profile.map(str::to_string),
        },
        version: "test".to_string(),
        config,
        disabled_reason: None,
    }
}

#[test]
fn remote_management_merges_enabled_user_names_but_upgrades_only_active_profile_git() {
    let mut system = user_layer(
        None,
        json!({"marketplaces": {"managed": {"source_type": "git"}}}),
    );
    system.name = ConfigLayerSource::System {
        file: LegacyAppPathString::from_string("/etc/codex/config.toml"),
    };
    let mut disabled = user_layer(
        None,
        json!({"marketplaces": {"disabled": {"source_type": "git"}}}),
    );
    disabled.disabled_reason = Some("disabled fixture".to_string());
    let metadata = MarketplaceManagement::from_layers(&[
        user_layer(
            Some("work"),
            json!({"marketplaces": {"profile": {"source_type": "git"}, "local": {"source_type": "local"}}}),
        ),
        disabled,
        user_layer(
            None,
            json!({"marketplaces": {"base": {"source_type": "git"}}}),
        ),
        system,
    ]);
    assert_eq!(
        metadata.user_configured,
        HashSet::from([
            "base".to_string(),
            "profile".to_string(),
            "local".to_string()
        ])
    );
    assert_eq!(
        metadata.active_user_git,
        HashSet::from(["profile".to_string()])
    );
}

#[test]
fn empty_active_profile_does_not_inherit_upgrade_authority() {
    let metadata = MarketplaceManagement::from_layers(&[
        user_layer(Some("work"), json!({})),
        user_layer(
            None,
            json!({"marketplaces": {"base": {"source_type": "git"}}}),
        ),
    ]);
    assert!(metadata.is_user_configured("base"));
    assert!(!metadata.is_user_configured_git("base"));
    assert!(!metadata.is_user_configured("windows-only"));
}
