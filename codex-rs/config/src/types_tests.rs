use super::*;
use pretty_assertions::assert_eq;

#[test]
fn tui_footer_defaults_to_disabled_single_borderless_row() {
    let footer: TuiFooter = toml::from_str("").expect("empty footer should deserialize");

    assert!(!footer.enabled);
    assert_eq!(footer.max_rows, 1);
    assert_eq!(footer.border, TuiFooterBorder::None);
    assert_eq!(footer.layout, TuiFooterLayout::Stacked);
    assert!(footer.adapter_ids.is_empty());
}

#[test]
fn tui_footer_accepts_aliases_and_explicit_layout() {
    let footer: TuiFooter = toml::from_str(
        r#"
enabled = true
max_rows = 3
border_style = "rounded"
layout = "compact"
adapters = ["account", "thread"]
"#,
    )
    .expect("footer aliases should deserialize");

    assert!(footer.enabled);
    assert_eq!(footer.max_rows, 3);
    assert_eq!(footer.border, TuiFooterBorder::Rounded);
    assert_eq!(footer.layout, TuiFooterLayout::Compact);
    assert_eq!(
        footer.adapter_ids,
        vec!["account".to_string(), "thread".to_string()]
    );
}

#[test]
fn deserialize_skill_config_with_name_selector() {
    let cfg: SkillConfig = toml::from_str(
        r#"
            name = "github:yeet"
            enabled = false
        "#,
    )
    .expect("should deserialize skill config with name selector");

    assert_eq!(cfg.name.as_deref(), Some("github:yeet"));
    assert_eq!(cfg.path, None);
    assert!(!cfg.enabled);
}

#[test]
fn deserialize_skill_config_with_path_selector() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let skill_path = tempdir.path().join("skills").join("demo").join("SKILL.md");
    let cfg: SkillConfig = toml::from_str(&format!(
        r#"
            path = {path:?}
            enabled = false
        "#,
        path = skill_path.display().to_string(),
    ))
    .expect("should deserialize skill config with path selector");

    assert_eq!(
        cfg,
        SkillConfig {
            path: Some(
                AbsolutePathBuf::from_absolute_path(&skill_path)
                    .expect("skill path should be absolute"),
            ),
            name: None,
            enabled: false,
        }
    );
}

#[test]
fn memories_config_clamps_count_limits_to_nonzero_values() {
    let config = MemoriesConfig::from(MemoriesToml {
        max_raw_memories_for_consolidation: Some(0),
        max_rollouts_per_startup: Some(0),
        ..Default::default()
    });

    assert_eq!(
        config,
        MemoriesConfig {
            max_raw_memories_for_consolidation: 1,
            max_rollouts_per_startup: 1,
            ..MemoriesConfig::default()
        }
    );
}

#[test]
fn memories_config_clamps_rate_limit_remaining_threshold() {
    let config = MemoriesConfig::from(MemoriesToml {
        min_rate_limit_remaining_percent: Some(101),
        ..Default::default()
    });
    assert_eq!(
        config,
        MemoriesConfig {
            min_rate_limit_remaining_percent: 100,
            ..MemoriesConfig::default()
        }
    );

    let config = MemoriesConfig::from(MemoriesToml {
        min_rate_limit_remaining_percent: Some(-1),
        ..Default::default()
    });
    assert_eq!(
        config,
        MemoriesConfig {
            min_rate_limit_remaining_percent: 0,
            ..MemoriesConfig::default()
        }
    );
}
