use std::collections::HashSet;
use std::fs;
use std::path::Component;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde_json::Value;

const MAX_COMMANDS_PER_PLUGIN: usize = 128;
const MAX_ID_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 512;
const MAX_PROMPT_LEN: usize = 16 * 1024;
const MAX_MCP_ARGUMENTS_LEN: usize = 64 * 1024;
const MAX_FIXED_ARGUMENTS: usize = 64;
const MAX_FIXED_ARGUMENT_LEN: usize = 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct PluginCommandContribution {
    pub id: String,
    pub name: String,
    pub description: String,
    pub target: PluginCommandTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PluginCommandTarget {
    Prompt {
        prompt: String,
    },
    McpTool {
        server: String,
        tool: String,
        arguments: Option<Value>,
    },
    Action(PluginCommandAction),
    Executable {
        package_root: AbsolutePathBuf,
        path: AbsolutePathBuf,
        argv: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PluginCommandAction {
    GoalGet,
    GoalSet {
        objective: Option<String>,
        status: Option<PluginGoalStatus>,
        token_budget: Option<Option<i64>>,
    },
    GoalClear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginGoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandManifest {
    #[serde(default)]
    contributions: CommandContributions,
    #[serde(default)]
    extensions: CommandExtensions,
}

#[derive(Default, Deserialize)]
struct CommandContributions {
    #[serde(default)]
    commands: Vec<RawCommand>,
}

#[derive(Default, Deserialize)]
struct CommandExtensions {
    #[serde(default, rename = "com.openai")]
    openai: OpenAiExtension,
}

#[derive(Default, Deserialize)]
struct OpenAiExtension {
    #[serde(default)]
    contributions: CommandContributions,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommand {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    target: RawTarget,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
enum RawTarget {
    Prompt {
        prompt: String,
    },
    McpTool {
        server: String,
        tool: String,
        arguments: Option<Value>,
    },
    Action {
        action: RawAction,
        objective: Option<String>,
        status: Option<PluginGoalStatus>,
        token_budget: Option<Option<i64>>,
    },
    Executable {
        path: String,
        #[serde(default)]
        argv: Vec<String>,
    },
}

#[derive(Deserialize)]
enum RawAction {
    #[serde(rename = "goalGet")]
    Get,
    #[serde(rename = "goalSet")]
    Set,
    #[serde(rename = "goalClear")]
    Clear,
}

/// Loads structured command contributions without interpreting the legacy
/// top-level `commands` migration field.
pub fn load_plugin_command_contributions(
    plugin_root: &Path,
) -> Result<Vec<PluginCommandContribution>> {
    let agent_manifest_path = plugin_root.join("plugin.json");
    let overlay_path = plugin_root.join(".codex-plugin/plugin.json");
    let (manifest_path, use_extension) = if agent_manifest_path.is_file() {
        (agent_manifest_path, true)
    } else {
        (overlay_path.clone(), false)
    };
    if !manifest_path.is_file() {
        return Ok(Vec::new());
    }

    let manifest: CommandManifest = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let raw_commands = if use_extension {
        let commands = manifest.extensions.openai.contributions.commands;
        if commands.is_empty() && overlay_path.is_file() {
            let overlay: CommandManifest = serde_json::from_str(
                &fs::read_to_string(&overlay_path)
                    .with_context(|| format!("failed to read {}", overlay_path.display()))?,
            )
            .with_context(|| format!("failed to parse {}", overlay_path.display()))?;
            overlay.contributions.commands
        } else {
            commands
        }
    } else {
        manifest.contributions.commands
    };
    if raw_commands.len() > MAX_COMMANDS_PER_PLUGIN {
        bail!("plugin declares too many command contributions");
    }

    let mut ids = HashSet::new();
    if raw_commands.iter().any(|command| !ids.insert(&command.id)) {
        bail!("plugin declares duplicate command ids");
    }
    raw_commands
        .into_iter()
        .map(|command| normalize_command(plugin_root, command))
        .collect()
}

fn normalize_command(plugin_root: &Path, command: RawCommand) -> Result<PluginCommandContribution> {
    validate_segment("command id", &command.id)?;
    validate_segment("command name", &command.name)?;
    if command.description.chars().count() > MAX_DESCRIPTION_LEN {
        bail!("command description exceeds {MAX_DESCRIPTION_LEN} characters");
    }

    let target = match command.target {
        RawTarget::Prompt { prompt } => {
            if prompt.is_empty() || prompt.chars().count() > MAX_PROMPT_LEN {
                bail!("command prompt must contain 1..={MAX_PROMPT_LEN} characters");
            }
            PluginCommandTarget::Prompt { prompt }
        }
        RawTarget::McpTool {
            server,
            tool,
            arguments,
        } => {
            validate_segment("MCP server", &server)?;
            validate_segment("MCP tool", &tool)?;
            let arguments_len = arguments
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()?
                .map_or(0, |serialized| serialized.len());
            if arguments_len > MAX_MCP_ARGUMENTS_LEN {
                bail!("MCP arguments exceed {MAX_MCP_ARGUMENTS_LEN} serialized bytes");
            }
            PluginCommandTarget::McpTool {
                server,
                tool,
                arguments,
            }
        }
        RawTarget::Action {
            action,
            objective,
            status,
            token_budget,
        } => PluginCommandTarget::Action(match action {
            RawAction::Get => PluginCommandAction::GoalGet,
            RawAction::Set => PluginCommandAction::GoalSet {
                objective,
                status,
                token_budget,
            },
            RawAction::Clear => PluginCommandAction::GoalClear,
        }),
        RawTarget::Executable { path, argv } => {
            let (package_root, path) = resolve_executable(plugin_root, &path)?;
            PluginCommandTarget::Executable {
                package_root,
                path,
                argv: validate_argv(argv)?,
            }
        }
    };

    Ok(PluginCommandContribution {
        id: command.id,
        name: command.name,
        description: command.description,
        target,
    })
}

fn validate_segment(label: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !matches!(value, "." | "..")
        && !value.starts_with(['-', '_', '.'])
        && !value.ends_with(['-', '_', '.']);
    if !valid {
        bail!("invalid {label}");
    }
    Ok(())
}

fn resolve_executable(
    plugin_root: &Path,
    relative_path: &str,
) -> Result<(AbsolutePathBuf, AbsolutePathBuf)> {
    let relative_path = Path::new(relative_path);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("executable path must be package-relative");
    }
    let root = plugin_root
        .canonicalize()
        .context("failed to resolve plugin root")?;
    let executable = root
        .join(relative_path)
        .canonicalize()
        .context("failed to resolve plugin executable")?;
    if !executable.starts_with(&root) || !executable.is_file() {
        bail!("plugin executable must be a file inside the plugin package");
    }
    Ok((
        AbsolutePathBuf::from_absolute_path_checked(root).context("plugin root is not absolute")?,
        AbsolutePathBuf::from_absolute_path_checked(executable)
            .context("plugin executable is not absolute")?,
    ))
}

fn validate_argv(argv: Vec<String>) -> Result<Vec<String>> {
    if argv.len() > MAX_FIXED_ARGUMENTS
        || argv
            .iter()
            .any(|argument| argument.len() > MAX_FIXED_ARGUMENT_LEN || argument.contains('\0'))
    {
        bail!("plugin executable argv exceeds its fixed bounds");
    }
    Ok(argv)
}

#[cfg(test)]
#[path = "command_contributions_tests.rs"]
mod tests;
