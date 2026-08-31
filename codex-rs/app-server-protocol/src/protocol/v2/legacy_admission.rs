use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Transition state for one Relay-managed legacy app-server admission seal.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum LegacyAdmissionState {
    Sealing,
    Drained,
    Aborted,
}

/// Sanitized state for a cutover-only legacy admission seal.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LegacyAdmissionSnapshot {
    pub cutover_epoch: String,
    pub app_server_instance_generation: String,
    pub state: LegacyAdmissionState,
    #[ts(type = "number")]
    pub in_flight_mutation_count: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LegacyAdmissionSealParams {
    pub cutover_epoch: String,
    pub expected_app_server_instance_generation: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LegacyAdmissionSealResponse {
    pub admission: LegacyAdmissionSnapshot,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LegacyAdmissionStatusParams {
    pub cutover_epoch: String,
    pub expected_app_server_instance_generation: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LegacyAdmissionStatusResponse {
    pub admission: LegacyAdmissionSnapshot,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LegacyAdmissionAbortParams {
    pub cutover_epoch: String,
    pub expected_app_server_instance_generation: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LegacyAdmissionAbortResponse {
    pub admission: LegacyAdmissionSnapshot,
}
