use crate::error_code::invalid_request;
use crate::transport::AppServerTransport;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::LegacyAdmissionAbortParams;
use codex_app_server_protocol::LegacyAdmissionAbortResponse;
use codex_app_server_protocol::LegacyAdmissionSealParams;
use codex_app_server_protocol::LegacyAdmissionSealResponse;
use codex_app_server_protocol::LegacyAdmissionSnapshot;
use codex_app_server_protocol::LegacyAdmissionState;
use codex_app_server_protocol::LegacyAdmissionStatusParams;
use codex_app_server_protocol::LegacyAdmissionStatusResponse;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Notify;

const RELAY_APP_SERVER_INSTANCE_ENV: &str = "LLC_RELAY_CODEX_APP_SERVER_INSTANCE";
const UNSUPPORTED_MESSAGE: &str =
    "legacy admission is only available to a Relay-managed legacy local app-server";

#[derive(Clone, Default)]
pub(crate) struct LegacyAdmissionGate {
    inner: Option<Arc<LegacyAdmissionInner>>,
}

struct LegacyAdmissionInner {
    app_server_instance_generation: String,
    state: Mutex<LegacyAdmissionGateState>,
    state_changed: Notify,
}

#[derive(Default)]
struct LegacyAdmissionGateState {
    seal: Option<LegacyAdmissionSeal>,
    in_flight_mutation_count: u64,
}

struct LegacyAdmissionSeal {
    cutover_epoch: String,
    state: LegacyAdmissionState,
}

pub(crate) struct LegacyAdmissionPermit {
    inner: Option<Arc<LegacyAdmissionInner>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestAdmission {
    Control,
    Mutation,
    ReadOrCompletion,
}

impl LegacyAdmissionGate {
    pub(crate) fn for_relay_legacy_process(transport: &AppServerTransport) -> Self {
        if !matches!(transport, AppServerTransport::UnixSocket { .. }) {
            return Self::default();
        }
        let Some(app_server_instance_generation) = std::env::var_os(RELAY_APP_SERVER_INSTANCE_ENV)
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.is_empty())
        else {
            return Self::default();
        };
        Self::enabled(app_server_instance_generation)
    }

    fn enabled(app_server_instance_generation: String) -> Self {
        Self {
            inner: Some(Arc::new(LegacyAdmissionInner {
                app_server_instance_generation,
                state: Mutex::new(LegacyAdmissionGateState::default()),
                state_changed: Notify::new(),
            })),
        }
    }

    pub(crate) fn admit(
        &self,
        request: &ClientRequest,
    ) -> Result<LegacyAdmissionPermit, JSONRPCErrorError> {
        if request_admission(request) != RequestAdmission::Mutation {
            return Ok(LegacyAdmissionPermit { inner: None });
        }
        let Some(inner) = &self.inner else {
            return Ok(LegacyAdmissionPermit { inner: None });
        };
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(seal) = &state.seal
            && matches!(
                seal.state,
                LegacyAdmissionState::Sealing | LegacyAdmissionState::Drained
            )
        {
            return Err(invalid_request(format!(
                "legacy admission is sealed for cutover epoch `{}`",
                seal.cutover_epoch
            )));
        }
        state.in_flight_mutation_count = state
            .in_flight_mutation_count
            .checked_add(1)
            .ok_or_else(|| invalid_request("legacy admission mutation count overflow"))?;
        Ok(LegacyAdmissionPermit {
            inner: Some(Arc::clone(inner)),
        })
    }

    pub(crate) fn accepts_client_response(&self) -> bool {
        let Some(inner) = &self.inner else {
            return true;
        };
        let state = inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.seal.as_ref().is_some_and(|seal| {
            matches!(
                seal.state,
                LegacyAdmissionState::Sealing | LegacyAdmissionState::Drained
            )
        })
    }

    pub(crate) async fn seal(
        &self,
        params: LegacyAdmissionSealParams,
    ) -> Result<LegacyAdmissionSealResponse, JSONRPCErrorError> {
        let inner = self.validated_inner(
            &params.cutover_epoch,
            &params.expected_app_server_instance_generation,
        )?;
        loop {
            let state_changed = inner.state_changed.notified();
            tokio::pin!(state_changed);
            state_changed.as_mut().enable();
            let snapshot = {
                let mut state = inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match &mut state.seal {
                    Some(seal) if seal.cutover_epoch != params.cutover_epoch => {
                        return Err(epoch_mismatch());
                    }
                    Some(_) => {}
                    None => {
                        state.seal = Some(LegacyAdmissionSeal {
                            cutover_epoch: params.cutover_epoch.clone(),
                            state: LegacyAdmissionState::Sealing,
                        });
                    }
                }
                mark_drained_if_idle(&mut state);
                snapshot(inner, &state)?
            };
            if snapshot.state != LegacyAdmissionState::Sealing {
                return Ok(LegacyAdmissionSealResponse {
                    admission: snapshot,
                });
            }
            state_changed.await;
        }
    }

    pub(crate) fn status(
        &self,
        params: LegacyAdmissionStatusParams,
    ) -> Result<LegacyAdmissionStatusResponse, JSONRPCErrorError> {
        let inner = self.validated_inner(
            &params.cutover_epoch,
            &params.expected_app_server_instance_generation,
        )?;
        let state = inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        validate_epoch(&state, &params.cutover_epoch)?;
        Ok(LegacyAdmissionStatusResponse {
            admission: snapshot(inner, &state)?,
        })
    }

    pub(crate) fn abort(
        &self,
        params: LegacyAdmissionAbortParams,
    ) -> Result<LegacyAdmissionAbortResponse, JSONRPCErrorError> {
        let inner = self.validated_inner(
            &params.cutover_epoch,
            &params.expected_app_server_instance_generation,
        )?;
        // Canonical commit has no app-server transition: Relay holds the exact
        // child lock through commit and then makes this legacy process unreachable.
        let response = {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            validate_epoch(&state, &params.cutover_epoch)?;
            let seal = state
                .seal
                .as_mut()
                .ok_or_else(|| invalid_request("legacy admission is not sealed"))?;
            seal.state = LegacyAdmissionState::Aborted;
            LegacyAdmissionAbortResponse {
                admission: snapshot(inner, &state)?,
            }
        };
        inner.state_changed.notify_waiters();
        Ok(response)
    }

    fn validated_inner(
        &self,
        cutover_epoch: &str,
        expected_app_server_instance_generation: &str,
    ) -> Result<&Arc<LegacyAdmissionInner>, JSONRPCErrorError> {
        if cutover_epoch.is_empty() {
            return Err(invalid_request("cutover epoch must not be empty"));
        }
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| invalid_request(UNSUPPORTED_MESSAGE))?;
        if inner.app_server_instance_generation != expected_app_server_instance_generation {
            return Err(invalid_request(
                "legacy app-server instance generation mismatch",
            ));
        }
        Ok(inner)
    }
}

impl LegacyAdmissionPermit {
    pub(crate) fn is_counted(&self) -> bool {
        self.inner.is_some()
    }
}

impl Drop for LegacyAdmissionPermit {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let notify = {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.in_flight_mutation_count = state.in_flight_mutation_count.saturating_sub(1);
            mark_drained_if_idle(&mut state)
        };
        if notify {
            inner.state_changed.notify_waiters();
        }
    }
}

fn mark_drained_if_idle(state: &mut LegacyAdmissionGateState) -> bool {
    if state.in_flight_mutation_count != 0 {
        return false;
    }
    let Some(seal) = state.seal.as_mut() else {
        return false;
    };
    if seal.state != LegacyAdmissionState::Sealing {
        return false;
    }
    seal.state = LegacyAdmissionState::Drained;
    true
}

fn validate_epoch(
    state: &LegacyAdmissionGateState,
    cutover_epoch: &str,
) -> Result<(), JSONRPCErrorError> {
    match &state.seal {
        Some(seal) if seal.cutover_epoch == cutover_epoch => Ok(()),
        Some(_) => Err(epoch_mismatch()),
        None => Err(invalid_request("legacy admission is not sealed")),
    }
}

fn epoch_mismatch() -> JSONRPCErrorError {
    invalid_request("legacy admission cutover epoch mismatch")
}

fn snapshot(
    inner: &LegacyAdmissionInner,
    state: &LegacyAdmissionGateState,
) -> Result<LegacyAdmissionSnapshot, JSONRPCErrorError> {
    let seal = state
        .seal
        .as_ref()
        .ok_or_else(|| invalid_request("legacy admission is not sealed"))?;
    Ok(LegacyAdmissionSnapshot {
        cutover_epoch: seal.cutover_epoch.clone(),
        app_server_instance_generation: inner.app_server_instance_generation.clone(),
        state: seal.state,
        in_flight_mutation_count: state.in_flight_mutation_count,
    })
}

fn request_admission(request: &ClientRequest) -> RequestAdmission {
    match request {
        ClientRequest::LegacyAdmissionSeal { .. }
        | ClientRequest::LegacyAdmissionStatus { .. }
        | ClientRequest::LegacyAdmissionAbort { .. } => RequestAdmission::Control,

        ClientRequest::Initialize { .. }
        | ClientRequest::ServerDiagnostics { .. }
        | ClientRequest::ThreadUnsubscribe { .. }
        | ClientRequest::ThreadAccountRotationRead { .. }
        | ClientRequest::ThreadRelinquish { .. }
        | ClientRequest::ThreadDecrementElicitation { .. }
        | ClientRequest::ThreadGoalGet { .. }
        | ClientRequest::ThreadQueueList { .. }
        | ClientRequest::ThreadBackgroundTerminalsClean { .. }
        | ClientRequest::ThreadBackgroundTerminalsList { .. }
        | ClientRequest::ThreadBackgroundTerminalsTerminate { .. }
        | ClientRequest::ThreadList { .. }
        | ClientRequest::ProjectList { .. }
        | ClientRequest::ProjectRead { .. }
        | ClientRequest::ThreadSectionList { .. }
        | ClientRequest::ThreadSearch { .. }
        | ClientRequest::ThreadSearchOccurrences { .. }
        | ClientRequest::ThreadLoadedList { .. }
        | ClientRequest::SessionRuntimeList { .. }
        | ClientRequest::ThreadRead { .. }
        | ClientRequest::ThreadTurnsList { .. }
        | ClientRequest::ThreadItemsList { .. }
        | ClientRequest::ThreadTimelineList { .. }
        | ClientRequest::SkillsList { .. }
        | ClientRequest::HooksList { .. }
        | ClientRequest::PluginList { .. }
        | ClientRequest::PluginCommandList { .. }
        | ClientRequest::PluginSearch { .. }
        | ClientRequest::PluginInstalled { .. }
        | ClientRequest::PluginRead { .. }
        | ClientRequest::PluginSkillRead { .. }
        | ClientRequest::PluginShareList { .. }
        | ClientRequest::AppsRead { .. }
        | ClientRequest::AppsList { .. }
        | ClientRequest::AppsInstalled { .. }
        | ClientRequest::FsReadFile { .. }
        | ClientRequest::FsGetMetadata { .. }
        | ClientRequest::FsReadDirectory { .. }
        | ClientRequest::FsUnwatch { .. }
        | ClientRequest::TurnInterrupt { .. }
        | ClientRequest::ThreadRealtimeStop { .. }
        | ClientRequest::ThreadRealtimeListVoices { .. }
        | ClientRequest::ModelList { .. }
        | ClientRequest::ModelProviderCapabilitiesRead { .. }
        | ClientRequest::ExperimentalFeatureList { .. }
        | ClientRequest::PermissionProfileList { .. }
        | ClientRequest::RemoteControlStatusRead { .. }
        | ClientRequest::RemoteControlPairingStatus { .. }
        | ClientRequest::RemoteControlClientsList { .. }
        | ClientRequest::CollaborationModeList { .. }
        | ClientRequest::EnvironmentInfo { .. }
        | ClientRequest::EnvironmentStatus { .. }
        | ClientRequest::McpServerStatusList { .. }
        | ClientRequest::McpResourceRead { .. }
        | ClientRequest::WindowsSandboxReadiness { .. }
        | ClientRequest::AccountSlotList { .. }
        | ClientRequest::AccountSlotRateLimitsRead { .. }
        | ClientRequest::AccountRotationRead { .. }
        | ClientRequest::BedrockDiscover { .. }
        | ClientRequest::CancelLoginAccount { .. }
        | ClientRequest::GetAccountRateLimits { .. }
        | ClientRequest::GetAccountTokenUsage { .. }
        | ClientRequest::GetWorkspaceMessages { .. }
        | ClientRequest::CommandExecTerminate { .. }
        | ClientRequest::ProcessKill { .. }
        | ClientRequest::ConfigRead { .. }
        | ClientRequest::ExternalAgentConfigDetect { .. }
        | ClientRequest::ExternalAgentConfigImportHistoriesRead { .. }
        | ClientRequest::ConfigRequirementsRead { .. }
        | ClientRequest::GetAccount { .. }
        | ClientRequest::GetConversationSummary { .. }
        | ClientRequest::GitDiffToRemote { .. }
        | ClientRequest::GetAuthStatus { .. }
        | ClientRequest::FuzzyFileSearch { .. }
        | ClientRequest::FuzzyFileSearchSessionStop { .. } => RequestAdmission::ReadOrCompletion,

        ClientRequest::ThreadStart { .. }
        | ClientRequest::ThreadResume { .. }
        | ClientRequest::ThreadFork { .. }
        | ClientRequest::ThreadArchive { .. }
        | ClientRequest::ThreadDelete { .. }
        | ClientRequest::ThreadAccountSwitch { .. }
        | ClientRequest::ThreadAccountRotationUpdate { .. }
        | ClientRequest::ThreadAccountRotationReset { .. }
        | ClientRequest::ThreadIncrementElicitation { .. }
        | ClientRequest::ThreadTransitionCommit { .. }
        | ClientRequest::ThreadApproveGuardianDeniedAction { .. }
        | ClientRequest::ThreadSetName { .. }
        | ClientRequest::ThreadGoalSet { .. }
        | ClientRequest::ThreadGoalClear { .. }
        | ClientRequest::ThreadGoalCreate { .. }
        | ClientRequest::ThreadGoalReplace { .. }
        | ClientRequest::ThreadQueueAdd { .. }
        | ClientRequest::ThreadQueueUpdate { .. }
        | ClientRequest::ThreadQueueDelete { .. }
        | ClientRequest::ThreadQueueReorder { .. }
        | ClientRequest::ThreadQueueStart { .. }
        | ClientRequest::ThreadMetadataUpdate { .. }
        | ClientRequest::ThreadSectionMove { .. }
        | ClientRequest::ThreadSettingsUpdate { .. }
        | ClientRequest::TurnSettingsUpdate { .. }
        | ClientRequest::ThreadMemoryModeSet { .. }
        | ClientRequest::MemoryReset { .. }
        | ClientRequest::ThreadUnarchive { .. }
        | ClientRequest::ThreadCompactStart { .. }
        | ClientRequest::ThreadShellCommand { .. }
        | ClientRequest::ThreadRollback { .. }
        | ClientRequest::ThreadRevert { .. }
        | ClientRequest::ProjectCreate { .. }
        | ClientRequest::ProjectImport { .. }
        | ClientRequest::ProjectUpdate { .. }
        | ClientRequest::ProjectMove { .. }
        | ClientRequest::ProjectDelete { .. }
        | ClientRequest::ThreadSectionCreate { .. }
        | ClientRequest::ThreadSectionUpdate { .. }
        | ClientRequest::ThreadSectionDelete { .. }
        | ClientRequest::ThreadInjectItems { .. }
        | ClientRequest::SkillsExtraRootsSet { .. }
        | ClientRequest::MarketplaceAdd { .. }
        | ClientRequest::MarketplaceRemove { .. }
        | ClientRequest::MarketplaceUpgrade { .. }
        | ClientRequest::PluginCommandInvoke { .. }
        | ClientRequest::PluginShareSave { .. }
        | ClientRequest::PluginShareUpdateTargets { .. }
        | ClientRequest::PluginShareCheckout { .. }
        | ClientRequest::PluginShareDelete { .. }
        | ClientRequest::FsWriteFile { .. }
        | ClientRequest::FsCreateDirectory { .. }
        | ClientRequest::FsRemove { .. }
        | ClientRequest::FsCopy { .. }
        | ClientRequest::FsWatch { .. }
        | ClientRequest::SkillsConfigWrite { .. }
        | ClientRequest::PluginInstall { .. }
        | ClientRequest::PluginUninstall { .. }
        | ClientRequest::PluginReconcile { .. }
        | ClientRequest::TurnStart { .. }
        | ClientRequest::ThreadPresentationAppend { .. }
        | ClientRequest::TurnSteer { .. }
        | ClientRequest::ThreadRealtimeStart { .. }
        | ClientRequest::ThreadRealtimeAppendAudio { .. }
        | ClientRequest::ThreadRealtimeAppendText { .. }
        | ClientRequest::ThreadRealtimeAppendSpeech { .. }
        | ClientRequest::ReviewStart { .. }
        | ClientRequest::ExperimentalFeatureEnablementSet { .. }
        | ClientRequest::RemoteControlEnable { .. }
        | ClientRequest::RemoteControlDisable { .. }
        | ClientRequest::RemoteControlPairingStart { .. }
        | ClientRequest::RemoteControlClientsRevoke { .. }
        | ClientRequest::MockExperimentalMethod { .. }
        | ClientRequest::EnvironmentAdd { .. }
        | ClientRequest::McpServerOauthLogin { .. }
        | ClientRequest::McpServerRefresh { .. }
        | ClientRequest::McpServerEventStreamStart { .. }
        | ClientRequest::McpServerEventStreamStop { .. }
        | ClientRequest::McpServerToolCall { .. }
        | ClientRequest::WindowsSandboxSetupStart { .. }
        | ClientRequest::AccountSlotLoginStart { .. }
        | ClientRequest::AccountSlotLogout { .. }
        | ClientRequest::AccountRotationUpdate { .. }
        | ClientRequest::LoginAccount { .. }
        | ClientRequest::BedrockSetup { .. }
        | ClientRequest::LogoutAccount { .. }
        | ClientRequest::ConsumeAccountRateLimitResetCredit { .. }
        | ClientRequest::SendAddCreditsNudgeEmail { .. }
        | ClientRequest::FeedbackUpload { .. }
        | ClientRequest::OneOffCommandExec { .. }
        | ClientRequest::CommandExecWrite { .. }
        | ClientRequest::CommandExecResize { .. }
        | ClientRequest::ProcessSpawn { .. }
        | ClientRequest::ProcessWriteStdin { .. }
        | ClientRequest::ProcessResizePty { .. }
        | ClientRequest::ExternalAgentConfigImport { .. }
        | ClientRequest::ExternalAgentConfigImportHistoryRecord { .. }
        | ClientRequest::ConfigValueWrite { .. }
        | ClientRequest::ConfigBatchWrite { .. }
        | ClientRequest::FuzzyFileSearchSessionStart { .. }
        | ClientRequest::FuzzyFileSearchSessionUpdate { .. } => RequestAdmission::Mutation,
    }
}

#[cfg(test)]
#[path = "legacy_admission_tests.rs"]
mod tests;
