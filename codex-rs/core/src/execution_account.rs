use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use codex_login::AuthManager;
use codex_models_manager::manager::SharedModelsManager;
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

/// Resolves host-managed account slots into immutable execution resources.
pub trait ExecutionAccountResolver: Send + Sync {
    fn resolve(&self, binding: ExecutionAccountBinding) -> ExecutionAccountResolverFuture<'_>;
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
