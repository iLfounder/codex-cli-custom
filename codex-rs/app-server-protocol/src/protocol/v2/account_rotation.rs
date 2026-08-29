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

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadAccountRotationSource {
    #[default]
    LegacyFixed,
    Global,
    Override,
}

/// The global rotation profile shared by inheriting threads.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountRotationSnapshot {
    pub mode: ThreadAccountRotationMode,
    pub fixed_account_slot_id: Option<String>,
    pub automatic_account_slot_ids: Vec<String>,
    #[ts(type = "number")]
    pub revision: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountRotationReadResponse {
    /// `null` means the global profile has not been activated yet.
    pub rotation: Option<AccountRotationSnapshot>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountRotationUpdateParams {
    /// Use revision zero to create the first global profile.
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
pub struct AccountRotationUpdateResponse {
    pub rotation: AccountRotationSnapshot,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountRotationChangedNotification {
    pub rotation: AccountRotationSnapshot,
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
    #[serde(default)]
    pub source: ThreadAccountRotationSource,
    /// The current global profile revision, or `null` before global activation.
    #[ts(type = "number | null")]
    pub global_profile_revision: Option<u64>,
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountRotationResetParams {
    pub thread_id: String,
    /// The exact thread-local override revision to remove.
    #[ts(type = "number")]
    pub expected_rotation_revision: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountRotationResetResponse {
    pub rotation: ThreadAccountRotationSnapshot,
}
