use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use super::ThreadGoal;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginCommandListParams {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginCommandListResponse {
    pub data: Vec<PluginCommand>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginCommand {
    pub id: String,
    pub plugin_id: String,
    pub canonical_name: String,
    pub short_name: Option<String>,
    pub description: String,
    pub target: PluginCommandTarget,
    pub available: bool,
    pub deny_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type", rename_all = "camelCase", export_to = "v2/")]
pub enum PluginCommandTarget {
    Prompt,
    McpTool { server: String, tool: String },
    Action { action: PluginCommandAction },
    Executable,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum PluginCommandAction {
    GoalGet,
    GoalSet,
    GoalClear,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginCommandInvokeParams {
    pub thread_id: String,
    pub command_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type", rename_all = "camelCase", export_to = "v2/")]
pub enum PluginCommandInvokeResponse {
    Prompt {
        prompt: String,
    },
    McpTool {
        result: PluginCommandMcpToolResult,
    },
    GoalGet {
        goal: Option<ThreadGoal>,
    },
    GoalSet {
        goal: ThreadGoal,
    },
    GoalClear {
        cleared: bool,
    },
    Executable {
        exit_code: Option<i32>,
        output: String,
        timed_out: bool,
    },
    Unavailable {
        deny_reason: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginCommandMcpToolResult {
    pub content: Vec<JsonValue>,
    pub structured_content: Option<JsonValue>,
    pub is_error: Option<bool>,
    #[serde(rename = "_meta")]
    #[ts(rename = "_meta")]
    pub meta: Option<JsonValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadPresentationAppendParams {
    pub thread_id: String,
    pub item: ThreadPresentation,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadPresentationAppendResponse {
    pub delivered_to: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadPresentationAppendedNotification {
    pub thread_id: String,
    pub item: ThreadPresentation,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type", rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadPresentation {
    Card {
        id: String,
        title: String,
        body: String,
    },
    Notice {
        id: String,
        level: ThreadPresentationNoticeLevel,
        message: String,
    },
    Progress {
        id: String,
        label: String,
        current: u64,
        total: Option<u64>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadPresentationNoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}
