use std::collections::HashMap;

use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadStartParams;
use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;
use serde_json::Value;

use crate::Cli;
use crate::legacy_core::config::Config;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CanonicalLaunchProjection {
    initial_account_number: Option<u32>,
    model: bool,
    oss: bool,
    service_tier: bool,
    approval_policy: bool,
    approvals_reviewer: bool,
    sandbox: bool,
    reasoning_effort: bool,
    reasoning_summary: bool,
    verbosity: bool,
    personality: bool,
    web_search: bool,
    bypass_hook_trust: bool,
}

impl CanonicalLaunchProjection {
    pub(crate) fn from_invocation(cli: &Cli, parsed_overrides: &[(String, toml::Value)]) -> Self {
        let has = |key: &str| parsed_overrides.iter().any(|(path, _)| path == key);
        Self {
            initial_account_number: None,
            model: cli.model.is_some() || cli.oss || has("model"),
            oss: cli.oss,
            service_tier: has("service_tier"),
            approval_policy: cli.approval_policy.is_some()
                || cli.dangerously_bypass_approvals_and_sandbox
                || has("approval_policy"),
            approvals_reviewer: has("approvals_reviewer"),
            sandbox: cli.sandbox_mode.is_some()
                || cli.dangerously_bypass_approvals_and_sandbox
                || has("sandbox_mode"),
            reasoning_effort: has("model_reasoning_effort"),
            reasoning_summary: has("model_reasoning_summary"),
            verbosity: has("model_verbosity"),
            personality: has("personality"),
            web_search: cli.web_search || has("web_search"),
            bypass_hook_trust: cli.bypass_hook_trust,
        }
    }

    pub(crate) fn with_managed_account_hint(mut self, hint: &str) -> std::io::Result<Self> {
        let number = hint
            .strip_prefix('C')
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|number| *number > 0 && hint == format!("C{number}"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "managed account hint is malformed",
                )
            })?;
        self.initial_account_number = Some(number);
        Ok(self)
    }

    pub(crate) fn has_explicit_overrides(self) -> bool {
        let Self {
            initial_account_number: _,
            model,
            oss,
            service_tier,
            approval_policy,
            approvals_reviewer,
            sandbox,
            reasoning_effort,
            reasoning_summary,
            verbosity,
            personality,
            web_search,
            bypass_hook_trust,
        } = self;
        model
            || oss
            || service_tier
            || approval_policy
            || approvals_reviewer
            || sandbox
            || reasoning_effort
            || reasoning_summary
            || verbosity
            || personality
            || web_search
            || bypass_hook_trust
    }

    pub(crate) fn validate_config(self, config: &Config) -> std::io::Result<()> {
        if !self.oss {
            return Ok(());
        }
        self.validate_oss_boundary(
            config.model_provider_id.as_str(),
            config.show_raw_agent_reasoning,
        )
    }

    fn validate_oss_boundary(
        self,
        provider_id: &str,
        show_raw_agent_reasoning: bool,
    ) -> std::io::Result<()> {
        if !matches!(
            provider_id,
            OLLAMA_OSS_PROVIDER_ID | LMSTUDIO_OSS_PROVIDER_ID
        ) || !show_raw_agent_reasoning
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed --oss requires a daemon-known built-in provider and raw-reasoning visibility; use --embedded for custom OSS configuration",
            ));
        }
        Ok(())
    }

    pub(crate) fn restrict_start(self, params: &mut ThreadStartParams) {
        params.initial_account_slot_id = self
            .initial_account_number
            .map(|number| format!("C{number}"));
        self.restrict_common(
            &mut params.model,
            &mut params.model_provider,
            &mut params.service_tier,
            &mut params.approval_policy,
            &mut params.approvals_reviewer,
            &mut params.sandbox,
            &mut params.permissions,
            &mut params.config,
            &mut params.developer_instructions,
        );
        params.ephemeral = None;
    }

    pub(crate) fn restrict_resume(self, params: &mut ThreadResumeParams) {
        self.restrict_common(
            &mut params.model,
            &mut params.model_provider,
            &mut params.service_tier,
            &mut params.approval_policy,
            &mut params.approvals_reviewer,
            &mut params.sandbox,
            &mut params.permissions,
            &mut params.config,
            &mut params.developer_instructions,
        );
        params.base_instructions = None;
    }

    pub(crate) fn restrict_fork(self, params: &mut ThreadForkParams) {
        self.restrict_common(
            &mut params.model,
            &mut params.model_provider,
            &mut params.service_tier,
            &mut params.approval_policy,
            &mut params.approvals_reviewer,
            &mut params.sandbox,
            &mut params.permissions,
            &mut params.config,
            &mut params.developer_instructions,
        );
        params.base_instructions = None;
        params.ephemeral = false;
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one common projection must clear the same wire fields on start, resume, and fork"
    )]
    fn restrict_common(
        self,
        model: &mut Option<String>,
        model_provider: &mut Option<String>,
        service_tier: &mut Option<Option<String>>,
        approval_policy: &mut Option<codex_app_server_protocol::AskForApproval>,
        approvals_reviewer: &mut Option<codex_app_server_protocol::ApprovalsReviewer>,
        sandbox: &mut Option<codex_app_server_protocol::SandboxMode>,
        permissions: &mut Option<String>,
        config: &mut Option<HashMap<String, Value>>,
        developer_instructions: &mut Option<String>,
    ) {
        if !self.model {
            *model = None;
        }
        if !self.oss {
            *model_provider = None;
        }
        if !self.service_tier {
            *service_tier = None;
        }
        if !self.approval_policy {
            *approval_policy = None;
        }
        if !self.approvals_reviewer {
            *approvals_reviewer = None;
        }
        if !self.sandbox {
            *sandbox = None;
            *permissions = None;
        }
        let mut overrides = config.take().unwrap_or_default();
        overrides.retain(|key, _| match key.as_str() {
            "model_reasoning_effort" => self.reasoning_effort,
            "model_reasoning_summary" => self.reasoning_summary,
            "model_verbosity" => self.verbosity,
            "personality" => self.personality,
            "web_search" => self.web_search,
            "bypass_hook_trust" => self.bypass_hook_trust,
            _ => false,
        });
        if self.oss {
            overrides.insert("show_raw_agent_reasoning".to_string(), Value::Bool(true));
        }
        *config = (!overrides.is_empty()).then_some(overrides);
        *developer_instructions = None;
    }
}

#[cfg(test)]
#[path = "canonical_launch_projection_tests.rs"]
mod tests;
