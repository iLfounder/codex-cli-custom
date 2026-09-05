use super::*;

#[test]
fn remote_invocation_keeps_only_explicit_allowlisted_values() {
    let mut cli = Cli::try_parse_from([
        "codex",
        "--model",
        "gpt-5.4",
        "--search",
        "-C",
        "/Volumes/work/repo",
    ])
    .expect("valid CLI");
    cli.config_overrides.raw_overrides = vec![
        "model_reasoning_effort=high".to_string(),
        "model_reasoning_summary=detailed".to_string(),
        "model_verbosity=low".to_string(),
        "service_tier=fast".to_string(),
    ];

    let overrides = capture_remote_invocation_overrides(&cli).expect("allowed remote invocation");

    assert_eq!(
        overrides.cwd().map(LegacyAppPathString::as_str),
        Some("/Volumes/work/repo")
    );
    assert_eq!(overrides.model(), Some("gpt-5.4"));
    assert_eq!(overrides.service_tier(), Some(Some("fast".to_string())));
    assert_eq!(
        overrides.config(),
        Some(HashMap::from([
            (
                "model_reasoning_effort".to_string(),
                serde_json::Value::String("high".to_string()),
            ),
            (
                "model_reasoning_summary".to_string(),
                serde_json::Value::String("detailed".to_string()),
            ),
            (
                "model_verbosity".to_string(),
                serde_json::Value::String("low".to_string()),
            ),
            (
                "web_search".to_string(),
                serde_json::Value::String("live".to_string()),
            ),
        ]))
    );
}

#[test]
fn remote_invocation_rejects_authority_changing_raw_config() {
    let mut cli = Cli::try_parse_from(["codex"]).expect("valid CLI");
    cli.config_overrides.raw_overrides =
        vec!["mcp_servers.filesystem.command=server-from-windows".to_string()];

    let error = capture_remote_invocation_overrides(&cli).expect_err("authority must be rejected");

    assert!(error.contains("-c mcp_servers.filesystem.command"));
    assert!(error.contains("remote host"));
}

#[test]
fn remote_invocation_rejects_relative_cwd_and_permission_flags() {
    let relative = Cli::try_parse_from(["codex", "-C", "relative/path"]).expect("valid CLI");
    assert!(
        capture_remote_invocation_overrides(&relative)
            .expect_err("relative cwd must be rejected")
            .contains("absolute server path")
    );

    let approval =
        Cli::try_parse_from(["codex", "--ask-for-approval", "never"]).expect("valid CLI");
    assert!(
        capture_remote_invocation_overrides(&approval)
            .expect_err("permission override must be rejected")
            .contains("--ask-for-approval")
    );
}

#[test]
fn remote_invocation_preserves_tilde_cwd_as_opaque_api_text() {
    let cli = Cli::try_parse_from(["codex", "-C", "~/repo/on/server"]).expect("valid CLI");

    let overrides = capture_remote_invocation_overrides(&cli).expect("allowed remote invocation");

    assert_eq!(
        overrides.cwd().map(LegacyAppPathString::as_str),
        Some("~/repo/on/server")
    );
}

#[test]
fn remote_invocation_rejects_non_exact_tilde_cwd_forms() {
    for cwd in ["~//repo", r"~/\repo"] {
        let cli = Cli::try_parse_from(["codex", "-C", cwd]).expect("valid CLI");

        let error = capture_remote_invocation_overrides(&cli)
            .expect_err("non-exact tilde cwd must be rejected");

        assert!(error.contains("exact `~`/`~/...` form"), "{error}");
    }
}
