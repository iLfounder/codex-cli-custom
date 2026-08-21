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
