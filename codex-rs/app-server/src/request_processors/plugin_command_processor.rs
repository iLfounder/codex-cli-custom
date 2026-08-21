use std::collections::HashMap;
use std::sync::Arc;

use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::PluginCommand;
use codex_app_server_protocol::PluginCommandAction as ApiCommandAction;
use codex_app_server_protocol::PluginCommandInvokeParams;
use codex_app_server_protocol::PluginCommandInvokeResponse;
use codex_app_server_protocol::PluginCommandListParams;
use codex_app_server_protocol::PluginCommandListResponse;
use codex_app_server_protocol::PluginCommandMcpToolResult;
use codex_app_server_protocol::PluginCommandTarget as ApiCommandTarget;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadGoalClearParams;
use codex_app_server_protocol::ThreadGoalGetParams;
use codex_app_server_protocol::ThreadGoalSetParams;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadPresentation;
use codex_app_server_protocol::ThreadPresentationAppendParams;
use codex_app_server_protocol::ThreadPresentationAppendResponse;
use codex_app_server_protocol::ThreadPresentationAppendedNotification;
use codex_core::ThreadManager;
use codex_core_plugins::PluginCommandAction;
use codex_core_plugins::PluginCommandContribution;
use codex_core_plugins::PluginCommandTarget;
use codex_core_plugins::PluginGoalStatus;
use codex_core_plugins::load_plugin_command_contributions;
use codex_protocol::ThreadId;
use sha2::Digest;
use sha2::Sha256;

use crate::config_manager::ConfigManager;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::OutgoingMessageSender;
use crate::thread_state::ThreadStateManager;

use super::ThreadGoalRequestProcessor;

const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 100;
const MAX_MCP_RESPONSE_LEN: usize = 64 * 1024;
const MAX_PRESENTATION_ID_LEN: usize = 128;
const MAX_PRESENTATION_TITLE_LEN: usize = 256;
const MAX_PRESENTATION_BODY_LEN: usize = 16 * 1024;

pub(crate) struct PluginCommandRequestProcessor {
    thread_manager: Arc<ThreadManager>,
    config_manager: ConfigManager,
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
    goal_processor: ThreadGoalRequestProcessor,
}

#[derive(Clone)]
struct ResolvedCommand {
    api: PluginCommand,
    target: PluginCommandTarget,
}

impl PluginCommandRequestProcessor {
    pub(crate) fn new(
        thread_manager: Arc<ThreadManager>,
        config_manager: ConfigManager,
        outgoing: Arc<OutgoingMessageSender>,
        thread_state_manager: ThreadStateManager,
        goal_processor: ThreadGoalRequestProcessor,
    ) -> Self {
        Self {
            thread_manager,
            config_manager,
            outgoing,
            thread_state_manager,
            goal_processor,
        }
    }

    pub(crate) async fn list(
        &self,
        params: PluginCommandListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let mut catalog = self.catalog(&params.thread_id).await?;
        let digest = catalog_digest(&catalog);
        let offset = parse_cursor(params.cursor.as_deref(), &digest)?;
        let limit = params
            .limit
            .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
            .unwrap_or(DEFAULT_PAGE_SIZE);
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(invalid_request(format!(
                "limit must be between 1 and {MAX_PAGE_SIZE}"
            )));
        }
        if offset > catalog.len() {
            return Err(invalid_request("invalid plugin command cursor"));
        }
        let catalog_len = catalog.len();
        let end = offset.saturating_add(limit).min(catalog_len);
        let data = catalog
            .drain(offset..end)
            .map(|command| command.api)
            .collect();
        let next_cursor = (end < catalog_len).then(|| format!("{digest}:{end}"));
        Ok(Some(PluginCommandListResponse { data, next_cursor }.into()))
    }

    pub(crate) async fn invoke(
        &self,
        params: PluginCommandInvokeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = params.thread_id;
        let command = self
            .catalog(&thread_id)
            .await?
            .into_iter()
            .find(|command| command.api.id == params.command_id)
            .ok_or_else(|| invalid_request("plugin command is unavailable or stale"))?;
        if !command.api.available {
            return Ok(Some(
                PluginCommandInvokeResponse::Unavailable {
                    deny_reason: command
                        .api
                        .deny_reason
                        .unwrap_or_else(|| "plugin command is unavailable".to_string()),
                }
                .into(),
            ));
        }

        let response = match command.target {
            PluginCommandTarget::Prompt { prompt } => {
                PluginCommandInvokeResponse::Prompt { prompt }
            }
            PluginCommandTarget::McpTool {
                server,
                tool,
                arguments,
            } => {
                let thread = self.loaded_thread(&thread_id).await?;
                let result = thread
                    .call_mcp_tool(&server, &tool, arguments, /*meta*/ None)
                    .await
                    .map_err(|error| internal_error(format!("{error:#}")))?;
                let result = PluginCommandMcpToolResult {
                    content: result.content,
                    structured_content: result.structured_content,
                    is_error: result.is_error,
                    meta: result.meta,
                };
                if serde_json::to_vec(&result)
                    .map_err(|error| internal_error(error.to_string()))?
                    .len()
                    > MAX_MCP_RESPONSE_LEN
                {
                    return Err(internal_error(format!(
                        "plugin MCP response exceeds {MAX_MCP_RESPONSE_LEN} serialized bytes"
                    )));
                }
                PluginCommandInvokeResponse::McpTool { result }
            }
            PluginCommandTarget::Action(action) => self.invoke_action(&thread_id, action).await?,
            PluginCommandTarget::Executable {
                package_root,
                path,
                argv,
            } => {
                let output = self
                    .loaded_thread(&thread_id)
                    .await?
                    .execute_plugin_executable(package_root, path, argv)
                    .await
                    .map_err(invalid_request)?;
                PluginCommandInvokeResponse::Executable {
                    exit_code: output.exit_code,
                    output: output.output,
                    timed_out: output.timed_out,
                }
            }
        };
        Ok(Some(response.into()))
    }

    pub(crate) async fn append_presentation(
        &self,
        params: ThreadPresentationAppendParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        validate_presentation(&params.item)?;
        let thread_id = parse_thread_id(&params.thread_id)?;
        self.thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;
        let subscribers = self
            .thread_state_manager
            .subscribed_connection_ids(thread_id)
            .await;
        if !subscribers.is_empty() {
            self.outgoing
                .send_server_notification_to_connections(
                    &subscribers,
                    ServerNotification::ThreadPresentationAppended(
                        ThreadPresentationAppendedNotification {
                            thread_id: thread_id.to_string(),
                            item: params.item,
                        },
                    ),
                )
                .await;
        }
        Ok(Some(
            ThreadPresentationAppendResponse {
                delivered_to: u32::try_from(subscribers.len()).unwrap_or(u32::MAX),
            }
            .into(),
        ))
    }

    async fn catalog(&self, thread_id: &str) -> Result<Vec<ResolvedCommand>, JSONRPCErrorError> {
        let thread = self.loaded_thread(thread_id).await?;
        let startup_config = thread.config().await;
        let config = self
            .config_manager
            .load_latest_config_for_thread(startup_config.as_ref())
            .await
            .map_err(|error| internal_error(format!("failed to reload config: {error}")))?;
        let execution_account = thread.execution_account();
        let services = self
            .thread_manager
            .execution_account_services(&execution_account);
        let outcome = services
            .plugins_manager
            .plugins_for_config(&config.plugins_config_input())
            .await;

        let mut commands = Vec::new();
        for plugin in outcome.plugins().iter().filter(|plugin| plugin.is_active()) {
            let Some(namespace) = plugin.plugin_namespace.as_deref() else {
                continue;
            };
            let contributions = match load_plugin_command_contributions(plugin.root.as_path()) {
                Ok(contributions) => contributions,
                Err(error) => {
                    tracing::warn!(plugin = %plugin.config_name, "invalid plugin commands: {error:#}");
                    continue;
                }
            };
            for contribution in contributions {
                let mcp_declared = match &contribution.target {
                    PluginCommandTarget::McpTool { server, .. } => {
                        plugin.mcp_servers.contains_key(server)
                    }
                    _ => true,
                };
                let available = mcp_declared;
                let deny_reason = if !mcp_declared {
                    Some("MCP server is not declared by this plugin".to_string())
                } else {
                    None
                };
                commands.push(resolved_command(
                    &plugin.config_name,
                    namespace,
                    contribution,
                    available,
                    deny_reason,
                ));
            }
        }
        commands.sort_by(|left, right| {
            left.api
                .canonical_name
                .cmp(&right.api.canonical_name)
                .then_with(|| left.api.id.cmp(&right.api.id))
        });
        assign_resolution_names(&mut commands);
        Ok(commands)
    }

    async fn loaded_thread(
        &self,
        thread_id: &str,
    ) -> Result<Arc<codex_core::CodexThread>, JSONRPCErrorError> {
        let thread_id = parse_thread_id(thread_id)?;
        self.thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))
    }

    async fn invoke_action(
        &self,
        thread_id: &str,
        action: PluginCommandAction,
    ) -> Result<PluginCommandInvokeResponse, JSONRPCErrorError> {
        match action {
            PluginCommandAction::GoalGet => self
                .goal_processor
                .plugin_goal_get(ThreadGoalGetParams {
                    thread_id: thread_id.to_string(),
                })
                .await
                .map(|response| PluginCommandInvokeResponse::GoalGet {
                    goal: response.goal,
                }),
            PluginCommandAction::GoalSet {
                objective,
                status,
                token_budget,
            } => self
                .goal_processor
                .plugin_goal_set(ThreadGoalSetParams {
                    thread_id: thread_id.to_string(),
                    objective,
                    status: status.map(api_goal_status),
                    token_budget,
                })
                .await
                .map(|response| PluginCommandInvokeResponse::GoalSet {
                    goal: response.goal,
                }),
            PluginCommandAction::GoalClear => self
                .goal_processor
                .plugin_goal_clear(ThreadGoalClearParams {
                    thread_id: thread_id.to_string(),
                })
                .await
                .map(|response| PluginCommandInvokeResponse::GoalClear {
                    cleared: response.cleared,
                }),
        }
    }
}

fn resolved_command(
    plugin_id: &str,
    namespace: &str,
    contribution: PluginCommandContribution,
    available: bool,
    deny_reason: Option<String>,
) -> ResolvedCommand {
    let mut hasher = Sha256::new();
    hasher.update(plugin_id.as_bytes());
    hasher.update([0]);
    hasher.update(contribution.id.as_bytes());
    let digest = hasher.finalize();
    let id = format!("pc_{}", encode_hex(&digest[..16]));
    let target = match &contribution.target {
        PluginCommandTarget::Prompt { .. } => ApiCommandTarget::Prompt,
        PluginCommandTarget::McpTool { server, tool, .. } => ApiCommandTarget::McpTool {
            server: server.clone(),
            tool: tool.clone(),
        },
        PluginCommandTarget::Action(action) => ApiCommandTarget::Action {
            action: match action {
                PluginCommandAction::GoalGet => ApiCommandAction::GoalGet,
                PluginCommandAction::GoalSet { .. } => ApiCommandAction::GoalSet,
                PluginCommandAction::GoalClear => ApiCommandAction::GoalClear,
            },
        },
        PluginCommandTarget::Executable { .. } => ApiCommandTarget::Executable,
    };
    ResolvedCommand {
        api: PluginCommand {
            id,
            plugin_id: plugin_id.to_string(),
            canonical_name: format!("/{namespace}:{}", contribution.name),
            short_name: None,
            description: contribution.description,
            target,
            available,
            deny_reason,
        },
        target: contribution.target,
    }
}

fn assign_resolution_names(commands: &mut [ResolvedCommand]) {
    let mut canonical_counts = HashMap::<String, usize>::new();
    let mut short_counts = HashMap::<String, usize>::new();
    for command in commands.iter() {
        *canonical_counts
            .entry(command.api.canonical_name.clone())
            .or_default() += 1;
        let short = command
            .api
            .canonical_name
            .rsplit_once(':')
            .map_or(command.api.canonical_name.as_str(), |(_, name)| name);
        *short_counts.entry(short.to_string()).or_default() += 1;
    }
    for command in commands {
        if canonical_counts[&command.api.canonical_name] != 1 {
            command.api.available = false;
            command.api.deny_reason = Some("canonical command name is ambiguous".to_string());
            continue;
        }
        let name = command
            .api
            .canonical_name
            .rsplit_once(':')
            .map_or(command.api.canonical_name.as_str(), |(_, name)| name);
        if short_counts[name] == 1 {
            command.api.short_name = Some(format!("/{name}"));
        }
    }
}

fn catalog_digest(commands: &[ResolvedCommand]) -> String {
    let mut hasher = Sha256::new();
    for command in commands {
        hasher.update(command.api.id.as_bytes());
        hasher.update([0]);
        hasher.update(command.api.canonical_name.as_bytes());
        hasher.update([0]);
    }
    encode_hex(&hasher.finalize()[..16])
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn parse_cursor(cursor: Option<&str>, digest: &str) -> Result<usize, JSONRPCErrorError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let Some((cursor_digest, offset)) = cursor.split_once(':') else {
        return Err(invalid_request("invalid plugin command cursor"));
    };
    if cursor_digest != digest {
        return Err(invalid_request("plugin command cursor is stale"));
    }
    offset
        .parse::<usize>()
        .map_err(|_| invalid_request("invalid plugin command cursor"))
}

fn parse_thread_id(thread_id: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(thread_id)
        .map_err(|error| invalid_request(format!("invalid thread id: {error}")))
}

fn api_goal_status(status: PluginGoalStatus) -> ThreadGoalStatus {
    match status {
        PluginGoalStatus::Active => ThreadGoalStatus::Active,
        PluginGoalStatus::Paused => ThreadGoalStatus::Paused,
        PluginGoalStatus::Blocked => ThreadGoalStatus::Blocked,
        PluginGoalStatus::UsageLimited => ThreadGoalStatus::UsageLimited,
        PluginGoalStatus::BudgetLimited => ThreadGoalStatus::BudgetLimited,
        PluginGoalStatus::Complete => ThreadGoalStatus::Complete,
    }
}

fn validate_presentation(item: &ThreadPresentation) -> Result<(), JSONRPCErrorError> {
    let (id, title, body) = match item {
        ThreadPresentation::Card { id, title, body } => (id, Some(title), body),
        ThreadPresentation::Notice { id, message, .. } => (id, None, message),
        ThreadPresentation::Progress {
            id,
            label,
            current,
            total,
        } => {
            if total.is_some_and(|total| *current > total) {
                return Err(invalid_request("presentation progress exceeds total"));
            }
            (id, None, label)
        }
    };
    if id.is_empty() || id.chars().count() > MAX_PRESENTATION_ID_LEN {
        return Err(invalid_request("invalid presentation id"));
    }
    if title.is_some_and(|title| title.chars().count() > MAX_PRESENTATION_TITLE_LEN)
        || body.is_empty()
        || body.chars().count() > MAX_PRESENTATION_BODY_LEN
    {
        return Err(invalid_request("presentation content exceeds its bounds"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "plugin_command_processor_tests.rs"]
mod tests;
