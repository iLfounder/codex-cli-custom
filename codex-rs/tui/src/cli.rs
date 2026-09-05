use clap::Args;
use clap::FromArgMatches;
use clap::Parser;
use codex_utils_cli::ApprovalModeCliArg;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_cli::SharedCliOptions;
use codex_utils_path_uri::LegacyAppPathString;
use std::collections::HashMap;

const REMOTE_CONFIG_ALLOWLIST: &[&str] = &[
    "model_reasoning_effort",
    "model_reasoning_summary",
    "model_verbosity",
    "web_search",
];

/// Explicit invocation values that a remote TUI may project onto the execution host.
///
/// This is captured before local configuration is loaded so Windows-side defaults cannot become
/// filesystem, provider, or permission authority for a remote app-server.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteInvocationOverrides {
    cwd: Option<LegacyAppPathString>,
    model: Option<String>,
    service_tier: Option<Option<String>>,
    config: HashMap<String, serde_json::Value>,
}

impl RemoteInvocationOverrides {
    pub fn cwd(&self) -> Option<&LegacyAppPathString> {
        self.cwd.as_ref()
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn service_tier(&self) -> Option<Option<String>> {
        self.service_tier.clone()
    }

    pub fn config(&self) -> Option<HashMap<String, serde_json::Value>> {
        (!self.config.is_empty()).then(|| self.config.clone())
    }

    pub fn has_lifecycle_overrides(&self) -> bool {
        self.cwd.is_some()
            || self.model.is_some()
            || self.service_tier.is_some()
            || !self.config.is_empty()
    }
}

#[derive(Parser, Clone, Debug)]
#[command(version)]
pub struct Cli {
    /// Optional user prompt to start the session.
    #[arg(value_name = "PROMPT", value_hint = clap::ValueHint::Other)]
    pub prompt: Option<String>,

    /// Error out when config.toml contains fields that are not recognized by this version of Codex.
    #[arg(long = "strict-config", default_value_t = false)]
    pub strict_config: bool,

    /// Start a new thread with an invocation-local embedded app server.
    ///
    /// Existing managed threads always remain owned by the canonical app server.
    #[arg(long = "embedded", default_value_t = false)]
    pub embedded: bool,

    // Internal controls set by the top-level `codex resume` subcommand.
    // These are not exposed as user flags on the base `codex` command.
    #[clap(skip)]
    pub resume_picker: bool,

    #[clap(skip)]
    pub resume_last: bool,

    /// Internal: resume a specific recorded session by id (UUID). Set by the
    /// top-level `codex resume <SESSION_ID>` wrapper; not exposed as a public flag.
    #[clap(skip)]
    pub resume_session_id: Option<String>,

    /// Internal: show all sessions (disables cwd filtering and shows CWD column).
    #[clap(skip)]
    pub resume_show_all: bool,

    /// Internal: include non-interactive sessions in resume listings.
    #[clap(skip)]
    pub resume_include_non_interactive: bool,

    /// Internal: open the daemon-wide agents overview instead of starting a thread.
    #[clap(skip)]
    pub agents_overview: bool,

    /// Explicit server-facing overrides captured by the top-level CLI for a remote invocation.
    #[clap(skip)]
    pub remote_invocation_overrides: Option<RemoteInvocationOverrides>,

    // Internal controls set by the top-level `codex fork` subcommand.
    // These are not exposed as user flags on the base `codex` command.
    #[clap(skip)]
    pub fork_picker: bool,

    #[clap(skip)]
    pub fork_last: bool,

    /// Internal: fork a specific recorded session by id (UUID). Set by the
    /// top-level `codex fork <SESSION_ID>` wrapper; not exposed as a public flag.
    #[clap(skip)]
    pub fork_session_id: Option<String>,

    /// Internal: show all sessions (disables cwd filtering and shows CWD column).
    #[clap(skip)]
    pub fork_show_all: bool,

    #[clap(flatten)]
    pub shared: TuiSharedCliOptions,

    /// Configure when the model requires human approval before executing a command.
    #[arg(long = "ask-for-approval", short = 'a')]
    pub approval_policy: Option<ApprovalModeCliArg>,

    /// Enable live web search. When enabled, the native Responses `web_search` tool is available to the model (no per‑call approval).
    #[arg(long = "search", default_value_t = false)]
    pub web_search: bool,

    /// Disable alternate screen mode
    ///
    /// Runs the TUI in inline mode, preserving terminal scrollback history.
    #[arg(long = "no-alt-screen", default_value_t = false)]
    pub no_alt_screen: bool,

    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,
}

/// Validate explicit remote-only invocation inputs and retain the narrow server-facing allowlist.
pub fn capture_remote_invocation_overrides(cli: &Cli) -> Result<RemoteInvocationOverrides, String> {
    let rejected_option = if !cli.add_dir.is_empty() {
        Some("--add-dir")
    } else if cli.approval_policy.is_some() {
        Some("--ask-for-approval")
    } else if cli.sandbox_mode.is_some() {
        Some("--sandbox")
    } else if cli.shared.auto_review {
        Some("--approve-for-me")
    } else if cli.dangerously_bypass_approvals_and_sandbox {
        Some("--dangerously-bypass-approvals-and-sandbox")
    } else if cli.bypass_hook_trust {
        Some("--dangerously-bypass-hook-trust")
    } else if cli.oss {
        Some("--oss")
    } else if cli.oss_provider.is_some() {
        Some("--local-provider")
    } else {
        None
    };
    if let Some(option) = rejected_option {
        return Err(remote_authority_error(option));
    }

    let cwd = cli
        .cwd
        .as_ref()
        .map(|cwd| {
            let cwd = LegacyAppPathString::from_path(cwd);
            let value = cwd.as_str();
            if is_exact_tilde_home_form(value) {
                return Ok(cwd);
            }
            if cwd.infer_absolute_path_convention().is_none() {
                return Err(
                    "remote `-C` must be an absolute server path or the exact `~`/`~/...` form"
                        .to_string(),
                );
            }
            Ok(cwd)
        })
        .transpose()?;

    let mut overrides = RemoteInvocationOverrides {
        cwd,
        model: cli.model.clone(),
        ..RemoteInvocationOverrides::default()
    };
    if cli.web_search {
        overrides.config.insert(
            "web_search".to_string(),
            serde_json::Value::String("live".to_string()),
        );
    }
    for (key, value) in cli.config_overrides.parse_overrides()? {
        if key == "service_tier" {
            overrides.service_tier = Some(match value {
                toml::Value::String(value) => Some(value),
                _ => {
                    return Err("remote `-c service_tier=...` requires a string value".to_string());
                }
            });
        } else if REMOTE_CONFIG_ALLOWLIST.contains(&key.as_str()) {
            let value = serde_json::to_value(value)
                .map_err(|error| format!("could not encode remote `-c {key}`: {error}"))?;
            overrides.config.insert(key, value);
        } else {
            return Err(remote_authority_error(&format!("-c {key}")));
        }
    }
    Ok(overrides)
}

pub(crate) fn is_exact_tilde_home_form(value: &str) -> bool {
    value == "~"
        || value
            .strip_prefix("~/")
            .is_some_and(|rest| !rest.starts_with('/') && !rest.starts_with('\\'))
}

fn remote_authority_error(option: &str) -> String {
    format!(
        "`{option}` cannot be applied by a remote TUI; configure filesystem, provider, permission, shell, network, feature, and trust authority on the remote host"
    )
}

impl std::ops::Deref for Cli {
    type Target = SharedCliOptions;

    fn deref(&self) -> &Self::Target {
        &self.shared.0
    }
}

impl std::ops::DerefMut for Cli {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.shared.0
    }
}

#[derive(Clone, Debug, Default)]
pub struct TuiSharedCliOptions(SharedCliOptions);

impl TuiSharedCliOptions {
    pub fn into_inner(self) -> SharedCliOptions {
        self.0
    }
}

impl std::ops::Deref for TuiSharedCliOptions {
    type Target = SharedCliOptions;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for TuiSharedCliOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Args for TuiSharedCliOptions {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        mark_tui_args(SharedCliOptions::augment_args(cmd))
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        mark_tui_args(SharedCliOptions::augment_args_for_update(cmd))
    }
}

impl FromArgMatches for TuiSharedCliOptions {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        SharedCliOptions::from_arg_matches(matches).map(Self)
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(matches)
    }
}

fn mark_tui_args(cmd: clap::Command) -> clap::Command {
    cmd.mut_arg("dangerously_bypass_approvals_and_sandbox", |arg| {
        arg.conflicts_with("approval_policy")
    })
    .mut_arg("auto_review", |arg| arg.conflicts_with("approval_policy"))
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
