use std::fs;

use pretty_assertions::assert_eq;

use super::*;

#[test]
fn loads_structured_commands_without_consuming_legacy_commands() {
    let root = tempfile::tempdir().expect("tempdir");
    fs::write(root.path().join("runner"), "fixture").expect("runner");
    fs::create_dir(root.path().join(".codex-plugin")).expect("manifest directory");
    fs::write(
        root.path().join(".codex-plugin/plugin.json"),
        r#"{
          "name": "fixture",
          "commands": ["legacy.md"],
          "contributions": {"commands": [
            {"id":"review","name":"review","description":"Review it","target":{"type":"prompt","prompt":"Review the current change."}},
            {"id":"run","name":"run","target":{"type":"executable","path":"runner","argv":["--fixed"]}}
          ]}
        }"#,
    )
    .expect("manifest");

    let commands = load_plugin_command_contributions(root.path()).expect("commands");

    assert_eq!(
        commands,
        vec![
            PluginCommandContribution {
                id: "review".to_string(),
                name: "review".to_string(),
                description: "Review it".to_string(),
                target: PluginCommandTarget::Prompt {
                    prompt: "Review the current change.".to_string(),
                },
            },
            PluginCommandContribution {
                id: "run".to_string(),
                name: "run".to_string(),
                description: String::new(),
                target: PluginCommandTarget::Executable {
                    package_root: AbsolutePathBuf::try_from(
                        root.path().canonicalize().expect("root"),
                    )
                    .expect("absolute root"),
                    path: AbsolutePathBuf::try_from(
                        root.path().join("runner").canonicalize().expect("path"),
                    )
                    .expect("absolute path"),
                    argv: vec!["--fixed".to_string()],
                },
            },
        ]
    );
}

#[test]
fn rejects_executable_escape() {
    let root = tempfile::tempdir().expect("tempdir");
    fs::create_dir(root.path().join(".codex-plugin")).expect("manifest directory");
    fs::write(
        root.path().join(".codex-plugin/plugin.json"),
        r#"{"contributions":{"commands":[{"id":"bad","name":"bad","target":{"type":"executable","path":"../outside"}}]}}"#,
    )
    .expect("manifest");

    let error = load_plugin_command_contributions(root.path()).expect_err("escape rejected");

    assert_eq!(
        error.to_string(),
        "executable path must be package-relative"
    );
}

#[test]
fn bounds_prompt_by_utf8_bytes() {
    let root = tempfile::tempdir().expect("tempdir");
    fs::create_dir(root.path().join(".codex-plugin")).expect("manifest directory");
    let prompt_at_limit = "é".repeat(MAX_PROMPT_BYTES / 2);
    fs::write(
        root.path().join(".codex-plugin/plugin.json"),
        serde_json::json!({
            "contributions": {"commands": [{
                "id": "bounded",
                "name": "bounded",
                "target": {"type": "prompt", "prompt": prompt_at_limit.clone()},
            }]},
        })
        .to_string(),
    )
    .expect("manifest");
    assert_eq!(
        load_plugin_command_contributions(root.path())
            .expect("prompt at byte limit")
            .len(),
        1
    );

    let prompt_over_limit = format!("{prompt_at_limit}a");
    fs::write(
        root.path().join(".codex-plugin/plugin.json"),
        serde_json::json!({
            "contributions": {"commands": [{
                "id": "oversized",
                "name": "oversized",
                "target": {"type": "prompt", "prompt": prompt_over_limit},
            }]},
        })
        .to_string(),
    )
    .expect("manifest");

    let error = load_plugin_command_contributions(root.path()).expect_err("oversized prompt");
    assert_eq!(
        error.to_string(),
        format!("command prompt must contain 1..={MAX_PROMPT_BYTES} UTF-8 bytes")
    );
}
