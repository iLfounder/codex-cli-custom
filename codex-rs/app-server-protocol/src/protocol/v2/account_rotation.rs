use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadAccountRotationMode {
    Fixed,
    QuotaAware,
    RoundRobin,
    ExhaustThenNext,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountRotationSnapshot {
    pub mode: ThreadAccountRotationMode,
    pub fixed_account_slot_id: Option<String>,
    pub automatic_account_slot_ids: Vec<String>,
    #[ts(type = "number")]
    pub revision: u64,
    pub last_committed_account_slot_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountRotationReadParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountRotationReadResponse {
    pub rotation: ThreadAccountRotationSnapshot,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountRotationUpdateParams {
    pub thread_id: String,
    #[ts(type = "number")]
    pub expected_rotation_revision: u64,
    pub mode: ThreadAccountRotationMode,
    #[ts(optional = nullable)]
    pub fixed_account_slot_id: Option<String>,
    pub automatic_account_slot_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountRotationUpdateResponse {
    pub rotation: ThreadAccountRotationSnapshot,
}
