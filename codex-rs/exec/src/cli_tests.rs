use super::*;
use pretty_assertions::assert_eq;

#[test]
fn resume_parses_prompt_after_global_flags() {
    const PROMPT: &str = "echo resume-with-global-flags-after-subcommand";
    let cli = Cli::parse_from([
        "codex-exec",
        "resume",
        "--last",
        "--json",
        "--model",
        "gpt-5.2-codex",
        "--dangerously-bypass-approvals-and-sandbox",
        "--skip-git-repo-check",
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        PROMPT,
    ]);

    assert!(cli.ephemeral);
    assert!(cli.ignore_user_config);
    assert!(cli.ignore_rules);
    let Some(Command::Resume(args)) = cli.command else {
        panic!("expected resume command");
    };
    let effective_prompt = args.prompt.clone().or_else(|| {
        if args.last {
            args.session_id.clone()
        } else {
            None
        }
    });
    assert_eq!(effective_prompt.as_deref(), Some(PROMPT));
}

#[test]
fn resume_accepts_output_flags_after_subcommand() {
    const PROMPT: &str = "echo resume-with-output-file";
    let cli = Cli::parse_from([
        "codex-exec",
        "resume",
        "session-123",
        "-o",
        "/tmp/resume-output.md",
        "--output-schema",
        "/tmp/schema.json",
        PROMPT,
    ]);

    assert_eq!(
        cli.last_message_file,
        Some(PathBuf::from("/tmp/resume-output.md"))
    );
    assert_eq!(cli.output_schema, Some(PathBuf::from("/tmp/schema.json")));
    let Some(Command::Resume(args)) = cli.command else {
        panic!("expected resume command");
    };
    assert_eq!(args.session_id.as_deref(), Some("session-123"));
    assert_eq!(args.prompt.as_deref(), Some(PROMPT));
}

#[test]
fn fork_parses_prompt_after_global_flags() {
    const PROMPT: &str = "continue on the fork";
    let cli = Cli::parse_from([
        "codex-exec",
        "fork",
        "session-123",
        "--json",
        "--model",
        "gpt-5.2-codex",
        "--skip-git-repo-check",
        "--ephemeral",
        PROMPT,
    ]);

    assert!(cli.json);
    assert!(cli.ephemeral);
    let Some(Command::Fork(args)) = cli.command else {
        panic!("expected fork command");
    };
    assert_eq!(args.session_id, "session-123");
    assert_eq!(args.prompt.as_deref(), Some(PROMPT));
}

#[test]
fn parses_config_isolation_flags() {
    let cli = Cli::parse_from([
        "codex-exec",
        "--ignore-user-config",
        "--ignore-rules",
        "summarize",
    ]);

    assert!(cli.ignore_user_config);
    assert!(cli.ignore_rules);
}

#[test]
fn account_failover_is_default_off_and_accepts_pre_semantic_opt_in() {
    let default_cli = Cli::parse_from(["codex-exec", "summarize"]);
    let opted_in_cli = Cli::parse_from([
        "codex-exec",
        "resume",
        "--last",
        "--account-failover=pre-semantic",
    ]);

    assert_eq!(default_cli.account_failover, AccountFailover::Disabled);
    assert_eq!(opted_in_cli.account_failover, AccountFailover::PreSemantic);
}

#[test]
fn account_rotation_is_optional_and_accepts_logical_modes() {
    let default_cli = Cli::parse_from(["codex-exec", "summarize"]);
    assert_eq!(default_cli.account_rotation, None);

    for (value, expected) in [
        ("quota-aware", AccountRotation::QuotaAware),
        ("round-robin", AccountRotation::RoundRobin),
        ("exhaust-then-next", AccountRotation::ExhaustThenNext),
    ] {
        let cli = Cli::parse_from([
            "codex-exec",
            "--account-failover=pre-semantic",
            "--account-rotation",
            value,
            "summarize",
        ]);
        assert_eq!(cli.account_rotation, Some(expected));
    }
}

#[test]
fn approve_for_me_flag_applies_to_resume_when_passed_at_exec_root() {
    for flag in ["--approve-for-me", "--not-so-yolo"] {
        let cli = Cli::parse_from(["codex-exec", flag, "resume", "--last"]);

        assert!(cli.auto_review);
    }
}

#[test]
fn approve_for_me_flag_conflicts_with_other_sandbox_modes() {
    for conflicting_args in [
        vec!["--sandbox", "read-only"],
        vec!["--dangerously-bypass-approvals-and-sandbox"],
    ] {
        let mut args = vec!["codex-exec", "--approve-for-me"];
        args.extend(conflicting_args);
        args.push("summarize");

        let error = Cli::try_parse_from(args).expect_err("flags should conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
