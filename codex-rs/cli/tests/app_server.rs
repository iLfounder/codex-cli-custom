use std::path::Path;

use anyhow::Result;
use app_test_support::app_server_json_shutdown_event;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

#[test]
fn strict_config_rejects_unknown_config_fields_for_app_server() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
foo = "bar"
"#,
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["app-server", "--strict-config", "--listen", "off"])
        .assert()
        .failure()
        .stderr(contains("unknown configuration field"));

    Ok(())
}

#[test]
fn agents_accept_interactive_configuration_overrides() -> Result<()> {
    let codex_home = TempDir::new()?;

    #[cfg(unix)]
    let accepted_args = [
        ["-c", "features.multi_agent_mode=true", "agents"].as_slice(),
        ["--enable", "multi_agent_mode", "agents"].as_slice(),
        ["--yolo", "agents"].as_slice(),
        ["--search", "agents"].as_slice(),
        ["--model", "gpt-5", "agents"].as_slice(),
        ["--approve-for-me", "agents"].as_slice(),
        ["--cd", ".", "agents"].as_slice(),
    ];
    #[cfg(not(unix))]
    let accepted_args = [
        ["-c", "model_reasoning_effort=\"high\"", "agents"].as_slice(),
        ["-c", "service_tier=\"fast\"", "agents"].as_slice(),
        ["--search", "agents"].as_slice(),
        ["--model", "gpt-5", "agents"].as_slice(),
        ["--cd", "~", "agents"].as_slice(),
    ];

    for args in accepted_args {
        let mut cmd = codex_command(codex_home.path())?;
        cmd.env("TERM", "xterm-256color").args(args);
        #[cfg(not(unix))]
        cmd.args(["--remote", "ws://127.0.0.1:4512"]);

        cmd.assert()
            .failure()
            .stderr(contains("stdin is not a terminal"));
    }

    Ok(())
}

#[test]
fn agents_reject_inputs_that_cannot_be_applied() -> Result<()> {
    let codex_home = TempDir::new()?;

    for (args, expected_error) in [
        (
            ["--image=image.png", "agents"].as_slice(),
            "does not accept an initial prompt or images",
        ),
        (
            ["--oss", "agents", "--remote", "ws://127.0.0.1:4512"].as_slice(),
            "cannot apply local provider or additional-directory overrides",
        ),
        (
            [
                "--add-dir",
                ".",
                "agents",
                "--remote",
                "ws://127.0.0.1:4512",
            ]
            .as_slice(),
            "cannot apply local provider or additional-directory overrides",
        ),
        (
            [
                "-c",
                "sandbox_workspace_write.writable_roots=[\"../shared\"]",
                "agents",
                "--remote",
                "ws://127.0.0.1:4512",
            ]
            .as_slice(),
            "cannot apply local provider or additional-directory overrides",
        ),
        (
            [
                "-c",
                "features.multi_agent_mode=true",
                "agents",
                "--remote",
                "ws://127.0.0.1:4512",
            ]
            .as_slice(),
            "cannot be applied by a remote TUI",
        ),
        (
            [
                "--enable",
                "multi_agent_mode",
                "agents",
                "--remote",
                "ws://127.0.0.1:4512",
            ]
            .as_slice(),
            "cannot be applied by a remote TUI",
        ),
        (
            [
                "--disable",
                "multi_agent_mode",
                "agents",
                "--remote",
                "ws://127.0.0.1:4512",
            ]
            .as_slice(),
            "cannot be applied by a remote TUI",
        ),
        (
            ["--yolo", "agents", "--remote", "ws://127.0.0.1:4512"].as_slice(),
            "cannot be applied by a remote TUI",
        ),
        (
            [
                "--approve-for-me",
                "agents",
                "--remote",
                "ws://127.0.0.1:4512",
            ]
            .as_slice(),
            "cannot be applied by a remote TUI",
        ),
        (
            ["--cd", ".", "agents", "--remote", "ws://127.0.0.1:4512"].as_slice(),
            "must be an absolute server path",
        ),
    ] {
        let mut cmd = codex_command(codex_home.path())?;
        cmd.args(args)
            .assert()
            .failure()
            .stderr(contains(expected_error));
    }

    Ok(())
}

#[test]
fn app_server_emits_json_info_events() -> Result<()> {
    let codex_home = TempDir::new()?;
    let event = app_server_json_shutdown_event("codex", &["app-server"], codex_home.path())?;

    assert_eq!(
        event,
        json!({
            "level": "INFO",
            "fields": {
                "message": "processor task exited",
                "exit_reason": "stdio_connection_closed",
                "remaining_connection_count": 0,
                "shutdown_forced": false,
            },
            "target": "codex_app_server",
        })
    );

    Ok(())
}
