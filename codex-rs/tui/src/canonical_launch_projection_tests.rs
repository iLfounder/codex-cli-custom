use std::collections::HashMap;

use clap::Parser;
use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::SandboxMode;
use codex_utils_path_uri::LegacyAppPathString;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn fully_populated_start() -> ThreadStartParams {
    ThreadStartParams {
        model: Some("model".to_string()),
        model_provider: Some("provider".to_string()),
        service_tier: Some(Some("fast".to_string())),
        cwd: Some(LegacyAppPathString::from_string("/work")),
        runtime_workspace_roots: Some(Vec::new()),
        approval_policy: Some(AskForApproval::Never),
        approvals_reviewer: Some(ApprovalsReviewer::AutoReview),
        sandbox: Some(SandboxMode::WorkspaceWrite),
        permissions: Some("workspace".to_string()),
        config: Some(HashMap::from([
            ("model_reasoning_effort".to_string(), json!("high")),
            ("model_reasoning_summary".to_string(), json!("detailed")),
            ("model_verbosity".to_string(), json!("high")),
            ("personality".to_string(), json!("friendly")),
            ("web_search".to_string(), json!("live")),
            ("bypass_hook_trust".to_string(), json!(true)),
            ("mcp_servers".to_string(), json!({"unsafe": true})),
        ])),
        developer_instructions: Some("numbered-home instructions".to_string()),
        ephemeral: Some(true),
        ..ThreadStartParams::default()
    }
}

#[test]
fn empty_projection_keeps_only_cwd_and_workspace_roots() {
    let mut actual = fully_populated_start();
    CanonicalLaunchProjection::default().restrict_start(&mut actual);

    assert_eq!(
        actual,
        ThreadStartParams {
            cwd: Some(LegacyAppPathString::from_string("/work")),
            runtime_workspace_roots: Some(Vec::new()),
            ..ThreadStartParams::default()
        }
    );
}

#[test]
fn selected_scalars_and_typed_fields_are_projected_without_unknown_config() {
    let cli = Cli::parse_from([
        "codex",
        "--model",
        "selected",
        "--ask-for-approval",
        "never",
        "--sandbox",
        "workspace-write",
        "--search",
    ]);
    let parsed = vec![
        (
            "model_reasoning_effort".to_string(),
            toml::Value::String("high".to_string()),
        ),
        (
            "personality".to_string(),
            toml::Value::String("friendly".to_string()),
        ),
    ];
    let projection = CanonicalLaunchProjection::from_invocation(&cli, &parsed);
    let mut actual = fully_populated_start();
    projection.restrict_start(&mut actual);

    let mut expected = fully_populated_start();
    expected.model_provider = None;
    expected.service_tier = None;
    expected.approvals_reviewer = None;
    expected.config = Some(HashMap::from([
        ("model_reasoning_effort".to_string(), json!("high")),
        ("personality".to_string(), json!("friendly")),
        ("web_search".to_string(), json!("live")),
    ]));
    expected.developer_instructions = None;
    expected.ephemeral = None;
    assert_eq!(actual, expected);
}

#[test]
fn empty_projection_restricts_resume_and_fork_to_canonical_safe_paths() {
    let config = Some(HashMap::from([(
        "model_reasoning_effort".to_string(),
        json!("high"),
    )]));
    let mut resume = ThreadResumeParams {
        thread_id: "thread".to_string(),
        model: Some("model".to_string()),
        model_provider: Some("custom".to_string()),
        cwd: Some(LegacyAppPathString::from_string("/work")),
        runtime_workspace_roots: Some(Vec::new()),
        approval_policy: Some(AskForApproval::Never),
        sandbox: Some(SandboxMode::WorkspaceWrite),
        config: config.clone(),
        base_instructions: Some("base".to_string()),
        developer_instructions: Some("developer".to_string()),
        ..ThreadResumeParams::default()
    };
    let mut fork = ThreadForkParams {
        thread_id: "thread".to_string(),
        model: Some("model".to_string()),
        model_provider: Some("custom".to_string()),
        cwd: Some(LegacyAppPathString::from_string("/work")),
        runtime_workspace_roots: Some(Vec::new()),
        approval_policy: Some(AskForApproval::Never),
        sandbox: Some(SandboxMode::WorkspaceWrite),
        config,
        base_instructions: Some("base".to_string()),
        developer_instructions: Some("developer".to_string()),
        ephemeral: true,
        ..ThreadForkParams::default()
    };

    CanonicalLaunchProjection::default().restrict_resume(&mut resume);
    CanonicalLaunchProjection::default().restrict_fork(&mut fork);

    assert_eq!(
        resume,
        ThreadResumeParams {
            thread_id: "thread".to_string(),
            cwd: Some(LegacyAppPathString::from_string("/work")),
            runtime_workspace_roots: Some(Vec::new()),
            ..ThreadResumeParams::default()
        }
    );
    assert_eq!(
        fork,
        ThreadForkParams {
            thread_id: "thread".to_string(),
            cwd: Some(LegacyAppPathString::from_string("/work")),
            runtime_workspace_roots: Some(Vec::new()),
            ..ThreadForkParams::default()
        }
    );
}

#[test]
fn oss_projection_keeps_only_provider_and_raw_reasoning_proof() {
    let cli = Cli::parse_from(["codex", "--oss"]);
    let projection = CanonicalLaunchProjection::from_invocation(&cli, &[]);
    let mut actual = fully_populated_start();
    projection.restrict_start(&mut actual);

    let mut expected = ThreadStartParams {
        model: Some("model".to_string()),
        model_provider: Some("provider".to_string()),
        cwd: Some(LegacyAppPathString::from_string("/work")),
        runtime_workspace_roots: Some(Vec::new()),
        config: Some(HashMap::from([(
            "show_raw_agent_reasoning".to_string(),
            json!(true),
        )])),
        ..ThreadStartParams::default()
    };
    expected.ephemeral = None;
    assert_eq!(actual, expected);
}

#[test]
fn managed_account_hint_binds_canonical_thread_start() {
    let cli = Cli::parse_from(["codex"]);
    let projection = CanonicalLaunchProjection::from_invocation(&cli, &[])
        .with_managed_account_hint("C3")
        .expect("managed account hint");
    let mut params = ThreadStartParams::default();

    projection.restrict_start(&mut params);

    assert_eq!(params.initial_account_slot_id.as_deref(), Some("C3"));
}

#[test]
fn oss_requires_built_in_provider_and_raw_reasoning_visibility() {
    let projection = CanonicalLaunchProjection {
        oss: true,
        ..CanonicalLaunchProjection::default()
    };
    for provider in [OLLAMA_OSS_PROVIDER_ID, LMSTUDIO_OSS_PROVIDER_ID] {
        projection
            .validate_oss_boundary(provider, /*show_raw_agent_reasoning*/ true)
            .expect("built-in OSS projection");
    }
    for (provider, show_raw_agent_reasoning) in [
        ("custom", true),
        (OLLAMA_OSS_PROVIDER_ID, false),
        (LMSTUDIO_OSS_PROVIDER_ID, false),
    ] {
        projection
            .validate_oss_boundary(provider, show_raw_agent_reasoning)
            .expect_err("unsafe OSS projection");
    }
}
