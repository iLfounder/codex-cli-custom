use std::sync::Arc;

use codex_mcp::McpResourceClient;
use codex_protocol::protocol::SessionSource;

use crate::ExtensionData;
use crate::ExtensionFuture;
use crate::ExtensionMetrics;

/// Inputs used to prepare extension-owned state for an execution-account switch.
pub struct ExecutionAccountRuntimePrepareInput<'a, C> {
    pub config: &'a C,
    pub session_source: &'a SessionSource,
    pub session_store: &'a ExtensionData,
    pub target_session_store: &'a ExtensionData,
    pub thread_store: &'a ExtensionData,
    pub mcp_resource_client: Arc<McpResourceClient>,
    pub extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
}

/// Prepared extension state that can be quiesced before and published after the host CAS.
pub trait PreparedExecutionAccountRuntime: Send + Sync {
    fn quiesce(&self) -> ExtensionFuture<'_, Result<(), String>>;

    /// Publishes only already-prepared state and must not block or fail.
    fn publish(&self, session_store: &ExtensionData, thread_store: &ExtensionData);
}

/// Extension hook for account-sensitive state that outlives one turn.
pub trait ExecutionAccountRuntimeContributor<C: Sync>: Send + Sync {
    fn prepare<'a>(
        &'a self,
        input: ExecutionAccountRuntimePrepareInput<'a, C>,
    ) -> ExtensionFuture<'a, Result<Arc<dyn PreparedExecutionAccountRuntime>, String>>;
}
