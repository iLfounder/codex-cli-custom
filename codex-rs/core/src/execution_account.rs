use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use codex_analytics::AnalyticsEventsClient;
use codex_core_plugins::PluginsManager;
use codex_login::AuthManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::ExecutionAccountBinding;

/// Immutable resources and durable provenance used to execute a thread or turn.
#[derive(Clone)]
pub struct ExecutionAccountContext {
    pub binding: ExecutionAccountBinding,
    pub auth_manager: Arc<AuthManager>,
    pub models_manager: SharedModelsManager,
}

/// Account-scoped plugin and MCP services tied to one exact auth runtime.
#[derive(Clone)]
pub struct ExecutionAccountServices {
    pub plugins_manager: Arc<PluginsManager>,
    pub mcp_manager: Arc<crate::mcp::McpManager>,
}

/// Inputs used to choose the execution account for one newly admitted user turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnExecutionAccountSelection {
    pub thread_id: ThreadId,
    pub current_binding: ExecutionAccountBinding,
    /// Opaque identity of the exact credential snapshot captured by this thread runtime.
    ///
    /// Selectors may compare it with host state but must not render or export it.
    pub credential_revision: Option<codex_login::CredentialRevision>,
}

/// Account choice made for one newly admitted user turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnExecutionAccountDecision {
    /// Keep the thread's currently published execution account.
    Keep,
    /// Select the slot under the returned policy revision.
    ///
    /// This remains distinct from [`Self::Keep`] when `target_slot_id` is the current slot: turn
    /// admission must retain `policy_revision`, while skipping an unnecessary runtime switch.
    Select {
        target_slot_id: String,
        policy_revision: u64,
    },
    /// Rebuild the current slot runtime under the returned policy revision.
    ///
    /// Hosts use this when the logical slot remains selected but its credentials or other
    /// host-owned runtime inputs changed. Core prepares a fresh runtime and advances the durable
    /// execution generation before starting the turn.
    ReprepareCurrent { policy_revision: u64 },
}

/// Future returned by [`TurnExecutionAccountSelector`] without async-trait machinery.
pub type TurnExecutionAccountSelectorFuture<'a> =
    Pin<Box<dyn Future<Output = CodexResult<TurnExecutionAccountDecision>> + Send + 'a>>;

/// Selects the execution account for an actual new user turn.
///
/// Implementations only choose a slot. Core remains responsible for resolving that slot,
/// preparing its runtime, and committing the durable thread binding.
pub trait TurnExecutionAccountSelector: Send + Sync {
    fn select(
        &self,
        selection: TurnExecutionAccountSelection,
    ) -> TurnExecutionAccountSelectorFuture<'_>;
}

pub(crate) struct PreparedTurnExecutionAccountTransition {
    resolved: ResolvedExecutionAccountTransition,
    services: ExecutionAccountServices,
}

impl PreparedTurnExecutionAccountTransition {
    pub(crate) fn new(
        resolved: ResolvedExecutionAccountTransition,
        services: ExecutionAccountServices,
    ) -> Self {
        Self { resolved, services }
    }

    pub(crate) fn execution_account(&self) -> &Arc<ExecutionAccountContext> {
        self.resolved.execution_account()
    }

    pub(crate) fn services(&self) -> &ExecutionAccountServices {
        &self.services
    }
}

pub(crate) type TurnExecutionAccountTransitionResolverFuture<'a> = Pin<
    Box<
        dyn Future<
                Output = Result<
                    PreparedTurnExecutionAccountTransition,
                    ExecutionAccountSwitchError,
                >,
            > + Send
            + 'a,
    >,
>;

/// Resolves one selected slot into the exact runtime and services needed by Core admission.
///
/// Implementations must keep any readiness lease alive in the returned prepared transition until
/// Core finishes the durable binding CAS and runtime publication.
pub(crate) trait TurnExecutionAccountTransitionResolver: Send + Sync {
    fn resolve(
        &self,
        current_binding: ExecutionAccountBinding,
        target_slot_id: String,
    ) -> TurnExecutionAccountTransitionResolverFuture<'_>;
}

pub(crate) struct DefaultTurnExecutionAccountSelector;

impl TurnExecutionAccountSelector for DefaultTurnExecutionAccountSelector {
    fn select(
        &self,
        _selection: TurnExecutionAccountSelection,
    ) -> TurnExecutionAccountSelectorFuture<'_> {
        Box::pin(async { Ok(TurnExecutionAccountDecision::Keep) })
    }
}

/// Fully prepared account-sensitive runtime published atomically for future turns.
///
/// A hot switch constructs this value before updating the durable binding. Existing turns retain
/// their captured runtime while the next turn observes the newly published bundle.
pub(crate) struct ExecutionAccountRuntime {
    pub(crate) execution_account: Arc<ExecutionAccountContext>,
    pub(crate) services: ExecutionAccountServices,
    pub(crate) mcp_runtime: Arc<codex_mcp::McpRuntime>,
    pub(crate) model_client: crate::client::ModelClient,
    pub(crate) analytics_events_client: AnalyticsEventsClient,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) network_proxy_audit_metadata: crate::config::NetworkProxyAuditMetadata,
    pub(crate) shell_snapshot: crate::shell_snapshot::ShellSnapshot,
    pub(crate) extension_runtimes:
        Vec<Arc<dyn codex_extension_api::PreparedExecutionAccountRuntime>>,
    pub(crate) guardian_review_session: Arc<crate::guardian::GuardianReviewSessionManager>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExecutionAccountSwitchError {
    #[error("target execution account is unavailable")]
    TargetUnavailable,
    #[error("target execution account runtime preparation failed")]
    PreparationFailed,
    #[error("execution account generation changed")]
    StaleGeneration,
    #[error("thread execution runtime is busy")]
    ThreadBusy,
    #[error("execution account binding persistence failed")]
    PersistenceFailed,
}

impl std::fmt::Debug for ExecutionAccountServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecutionAccountServices")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ExecutionAccountContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecutionAccountContext")
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}

/// Future returned by [`ExecutionAccountResolver`] without requiring async-trait machinery.
pub type ExecutionAccountResolverFuture<'a> =
    Pin<Box<dyn Future<Output = CodexResult<Arc<ExecutionAccountContext>>> + Send + 'a>>;

/// Resolved execution resources whose readiness remains valid for an account transition.
///
/// Host-managed resolvers can attach an opaque lease that prevents the target slot from becoming
/// unavailable while the thread prepares and durably commits its new account binding.
pub struct ResolvedExecutionAccountTransition {
    execution_account: Arc<ExecutionAccountContext>,
    _readiness_lease: Option<Box<dyn Send + 'static>>,
}

impl ResolvedExecutionAccountTransition {
    pub fn new(execution_account: Arc<ExecutionAccountContext>) -> Self {
        Self {
            execution_account,
            _readiness_lease: None,
        }
    }

    pub fn with_readiness_lease(
        execution_account: Arc<ExecutionAccountContext>,
        readiness_lease: impl Send + 'static,
    ) -> Self {
        Self {
            execution_account,
            _readiness_lease: Some(Box::new(readiness_lease)),
        }
    }

    pub fn execution_account(&self) -> &Arc<ExecutionAccountContext> {
        &self.execution_account
    }
}

/// Future returned by [`ExecutionAccountResolver::resolve_for_transition`].
pub type ExecutionAccountTransitionResolverFuture<'a> =
    Pin<Box<dyn Future<Output = CodexResult<ResolvedExecutionAccountTransition>> + Send + 'a>>;

/// Resolves host-managed account slots into immutable execution resources.
pub trait ExecutionAccountResolver: Send + Sync {
    /// Returns the binding used when a brand-new root thread has no inherited or stored binding.
    ///
    /// Resume, fork, and explicitly bound starts bypass this fallback. Host resolvers may override
    /// it to choose their process-local initial slot without reinterpreting stored bindings.
    fn initial_binding_for_new_thread(&self) -> ExecutionAccountBinding {
        ExecutionAccountBinding {
            slot_id: "default".to_string(),
            generation: 1,
        }
    }

    fn resolve(&self, binding: ExecutionAccountBinding) -> ExecutionAccountResolverFuture<'_>;

    fn resolve_for_transition(
        &self,
        binding: ExecutionAccountBinding,
    ) -> ExecutionAccountTransitionResolverFuture<'_> {
        let resolved = self.resolve(binding);
        Box::pin(async move { resolved.await.map(ResolvedExecutionAccountTransition::new) })
    }
}

pub(crate) struct DefaultExecutionAccountResolver {
    auth_manager: Arc<AuthManager>,
    models_manager: SharedModelsManager,
}

impl DefaultExecutionAccountResolver {
    pub(crate) fn new(auth_manager: Arc<AuthManager>, models_manager: SharedModelsManager) -> Self {
        Self {
            auth_manager,
            models_manager,
        }
    }
}

impl ExecutionAccountResolver for DefaultExecutionAccountResolver {
    fn resolve(&self, binding: ExecutionAccountBinding) -> ExecutionAccountResolverFuture<'_> {
        Box::pin(async move {
            if binding.slot_id != "default" {
                return Err(CodexErr::InvalidRequest(format!(
                    "execution account slot `{}` is unavailable",
                    binding.slot_id
                )));
            }
            Ok(Arc::new(ExecutionAccountContext {
                binding,
                auth_manager: Arc::clone(&self.auth_manager),
                models_manager: Arc::clone(&self.models_manager),
            }))
        })
    }
}

#[cfg(test)]
#[path = "execution_account_tests.rs"]
mod tests;
