use crate::JsonSchema;
use crate::TS;
use codex_utils_path_uri::LegacyAppPathString;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct FeedbackUploadParams {
    pub classification: String,
    #[ts(optional = nullable)]
    pub reason: Option<String>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_logs: bool,
    #[ts(optional = nullable)]
    pub extra_log_files: Option<Vec<LegacyAppPathString>>,
    #[ts(optional = nullable)]
    pub tags: Option<BTreeMap<String, String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct FeedbackUploadResponse {
    pub thread_id: String,
}
