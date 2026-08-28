use super::session::PreparedExecutionAccountRuntime;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::execution_account::ExecutionAccountSwitchError;
use crate::execution_account::PreparedTurnExecutionAccountTransition;
use crate::execution_account::SuccessfulAccountBindingTransition;
use crate::execution_account::TurnExecutionAccountDecision;
use crate::execution_account::TurnExecutionAccountFailoverSelection;
use crate::execution_account::TurnExecutionAccountSelection;
use crate::execution_account::TurnExecutionAccountSuccessCommit;
use codex_async_utils::OrCancelExt;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::ExecutionAccountBinding;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
pub(crate) enum AttemptEffect {
    Hook = 1,
    Compaction = 2,
    SemanticResponse = 3,
    ResponseCreated = 4,
}

struct AccountAttemptCandidate {
    binding: ExecutionAccountBinding,
    policy_revision: Option<u64>,
    binding_transition: SuccessfulAccountBindingTransition,
    resolved: Option<PreparedTurnExecutionAccountTransition>,
    prepared: Option<PreparedExecutionAccountRuntime>,
}

impl AccountAttemptCandidate {
    fn runtime(&self, session: &Session) -> Arc<crate::execution_account::ExecutionAccountRuntime> {
        self.prepared
            .as_ref()
            .map(|prepared| Arc::clone(&prepared.runtime))
            .unwrap_or_else(|| session.execution_account_runtime())
    }
}

struct AccountFailoverInner {
    initial_binding: ExecutionAccountBinding,
    credential_revision: Option<codex_login::CredentialRevision>,
    candidate: Mutex<AccountAttemptCandidate>,
    tried_slot_ids: Mutex<BTreeSet<String>>,
    effect: AtomicU8,
    committed: AtomicBool,
}

impl Drop for AccountFailoverInner {
    fn drop(&mut self) {
        let prepared = self
            .candidate
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prepared
            .take();
        if let Some(prepared) = prepared
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(cleanup_prepared_runtime(prepared));
        }
    }
}

/// Execution-local authority for one root turn's account attempts.
#[derive(Clone)]
pub(crate) struct PreSemanticAccountFailover {
    inner: Arc<AccountFailoverInner>,
}

pub(super) struct AccountFailoverCleanupGuard {
    failover: Option<PreSemanticAccountFailover>,
}

impl Drop for AccountFailoverCleanupGuard {
    fn drop(&mut self) {
        let Some(failover) = self.failover.take() else {
            return;
        };
        let prepared = failover.take_uncommitted_runtime();
        if let Some(prepared) = prepared
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(cleanup_prepared_runtime(prepared));
        }
    }
}

impl PreSemanticAccountFailover {
    pub(super) fn cleanup_guard(&self) -> AccountFailoverCleanupGuard {
        AccountFailoverCleanupGuard {
            failover: Some(self.clone()),
        }
    }

    pub(super) fn is_provisional(&self) -> bool {
        !self.inner.committed.load(Ordering::Acquire)
    }

    pub(crate) fn mark_effect(&self, effect: AttemptEffect) {
        let _ = self.inner.effect.compare_exchange(
            0,
            effect as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(super) fn has_irreversible_effect(&self) -> bool {
        self.inner.effect.load(Ordering::Acquire) != 0
    }

    pub(super) fn can_fail_over(&self, error: &CodexErr) -> bool {
        !self.inner.committed.load(Ordering::Acquire)
            && self.inner.effect.load(Ordering::Acquire) == 0
            && error.account_rejection_kind().is_some()
    }

    pub(super) fn turn_runtime(
        &self,
        session: &Session,
    ) -> Arc<crate::execution_account::ExecutionAccountRuntime> {
        self.inner
            .candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime(session)
    }

    pub(crate) fn hooks(&self, session: &Session) -> Arc<codex_hooks::Hooks> {
        self.inner
            .candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prepared
            .as_ref()
            .map(|prepared| Arc::clone(&prepared.hooks))
            .unwrap_or_else(|| session.hooks())
    }

    pub(super) fn accepted_binding(&self) -> ExecutionAccountBinding {
        self.inner
            .candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .binding
            .clone()
    }

    pub(super) fn policy_revision(&self) -> Option<u64> {
        self.inner
            .candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .policy_revision
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "successful account commit and runtime publication share one transition fence"
    )]
    pub(super) async fn commit_response_created(
        &self,
        session: &Arc<Session>,
        turn_context: &TurnContext,
    ) -> CodexResult<()> {
        self.mark_effect(AttemptEffect::ResponseCreated);
        if self.inner.committed.load(Ordering::Acquire) {
            return Ok(());
        }
        let _transition = session.execution_runtime_transition_lock.lock().await;
        if session.execution_account().binding != self.inner.initial_binding {
            return Err(CodexErr::InvalidRequest(
                "execution account binding changed before successful attempt commit".to_string(),
            ));
        }
        let (binding, policy_revision, binding_transition) = {
            let candidate = self
                .inner
                .candidate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                candidate.binding.clone(),
                candidate.policy_revision,
                candidate.binding_transition,
            )
        };
        let accepted = if let Some(policy_revision) = policy_revision {
            session
                .turn_execution_account_selector()
                .commit_successful_selection(TurnExecutionAccountSuccessCommit {
                    thread_id: session.thread_id(),
                    expected_binding: self.inner.initial_binding.clone(),
                    target_slot_id: binding.slot_id.clone(),
                    policy_revision,
                    binding_transition,
                })
                .await?
        } else {
            self.inner.initial_binding.clone()
        };
        if accepted != binding {
            return Err(CodexErr::InvalidRequest(
                "successful account commit returned an unexpected binding".to_string(),
            ));
        }
        let (target, prepared) = {
            let mut candidate = self
                .inner
                .candidate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let target = candidate
                .resolved
                .as_ref()
                .map(|resolved| Arc::clone(resolved.execution_account()));
            (target, candidate.prepared.take())
        };
        self.inner.committed.store(true, Ordering::Release);
        if let (Some(target), Some(prepared)) = (target, prepared) {
            session
                .publish_prepared_execution_account_runtime(target, prepared)
                .await;
        }
        session
            .persist_accepted_execution_account_context(turn_context)
            .await;
        Ok(())
    }

    pub(super) async fn prepare_next_turn_context(
        &self,
        session: &Arc<Session>,
        previous: &Arc<TurnContext>,
        error: &CodexErr,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<Arc<TurnContext>> {
        let rejection_kind = error.account_rejection_kind().ok_or_else(|| {
            CodexErr::InvalidRequest("account failover requires a typed rejection".to_string())
        })?;
        let rejected_slot_id = self
            .inner
            .candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .binding
            .slot_id
            .clone();
        if let Some(prepared) = self.take_uncommitted_runtime() {
            let mut cleanup = tokio::spawn(cleanup_prepared_runtime(prepared));
            tokio::select! {
                _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                _ = &mut cleanup => {}
            }
        }
        let excluded_account_slot_ids = self
            .inner
            .tried_slot_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let decision = session
            .turn_execution_account_selector()
            .select_failover(TurnExecutionAccountFailoverSelection {
                selection: TurnExecutionAccountSelection {
                    thread_id: session.thread_id(),
                    current_binding: self.inner.initial_binding.clone(),
                    credential_revision: self.inner.credential_revision.clone(),
                },
                rejected_slot_id,
                rejection_kind,
                excluded_account_slot_ids: excluded_account_slot_ids.clone(),
            })
            .or_cancel(cancellation_token)
            .await
            .map_err(|_| CodexErr::TurnAborted)??;
        let mut candidate = prepare_candidate(session, &self.inner.initial_binding, decision)
            .or_cancel(cancellation_token)
            .await
            .map_err(|_| CodexErr::TurnAborted)??;
        if cancellation_token.is_cancelled() {
            if let Some(prepared) = candidate.prepared.take() {
                tokio::spawn(cleanup_prepared_runtime(prepared));
            }
            return Err(CodexErr::TurnAborted);
        }
        if excluded_account_slot_ids.contains(&candidate.binding.slot_id) {
            return Err(CodexErr::InvalidRequest(
                "account failover selector repeated an attempted account".to_string(),
            ));
        }
        let runtime = candidate.runtime(session.as_ref());
        self.inner
            .tried_slot_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(candidate.binding.slot_id.clone());
        *self
            .inner
            .candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = candidate;
        let next = session
            .new_turn_context_for_account_attempt(previous, runtime)
            .await;
        next.extension_data.insert(self.clone());
        Ok(next)
    }

    fn take_uncommitted_runtime(&self) -> Option<PreparedExecutionAccountRuntime> {
        if self.inner.committed.load(Ordering::Acquire) {
            return None;
        }
        self.inner
            .candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prepared
            .take()
    }
}

pub(super) async fn prepare_initial(
    session: &Arc<Session>,
    initial_binding: ExecutionAccountBinding,
    decision: TurnExecutionAccountDecision,
) -> CodexResult<PreSemanticAccountFailover> {
    let candidate = prepare_candidate(session, &initial_binding, decision).await?;
    let tried_slot_ids = BTreeSet::from([candidate.binding.slot_id.clone()]);
    Ok(PreSemanticAccountFailover {
        inner: Arc::new(AccountFailoverInner {
            credential_revision: session
                .execution_account()
                .auth_manager
                .credential_revision(),
            initial_binding,
            candidate: Mutex::new(candidate),
            tried_slot_ids: Mutex::new(tried_slot_ids),
            effect: AtomicU8::new(0),
            committed: AtomicBool::new(false),
        }),
    })
}

async fn prepare_candidate(
    session: &Arc<Session>,
    initial_binding: &ExecutionAccountBinding,
    decision: TurnExecutionAccountDecision,
) -> CodexResult<AccountAttemptCandidate> {
    let (target_slot_id, policy_revision, binding_transition) = match decision {
        TurnExecutionAccountDecision::Keep => {
            return Ok(AccountAttemptCandidate {
                binding: initial_binding.clone(),
                policy_revision: None,
                binding_transition: SuccessfulAccountBindingTransition::Keep,
                resolved: None,
                prepared: None,
            });
        }
        TurnExecutionAccountDecision::Select {
            target_slot_id,
            policy_revision,
        } if target_slot_id == initial_binding.slot_id => {
            return Ok(AccountAttemptCandidate {
                binding: initial_binding.clone(),
                policy_revision: Some(policy_revision),
                binding_transition: SuccessfulAccountBindingTransition::Keep,
                resolved: None,
                prepared: None,
            });
        }
        TurnExecutionAccountDecision::Select {
            target_slot_id,
            policy_revision,
        } => (
            target_slot_id,
            policy_revision,
            SuccessfulAccountBindingTransition::AdvanceGeneration,
        ),
        TurnExecutionAccountDecision::ReprepareCurrent { policy_revision } => (
            initial_binding.slot_id.clone(),
            policy_revision,
            SuccessfulAccountBindingTransition::AdvanceGeneration,
        ),
    };
    let resolver = session
        .turn_execution_account_transition_resolver()
        .ok_or_else(|| {
            CodexErr::InvalidRequest(ExecutionAccountSwitchError::PreparationFailed.to_string())
        })?;
    let resolved = resolver
        .resolve(initial_binding.clone(), target_slot_id.clone())
        .await
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
    let expected_binding = ExecutionAccountBinding {
        slot_id: target_slot_id,
        generation: initial_binding.generation.checked_add(1).ok_or_else(|| {
            CodexErr::InvalidRequest(ExecutionAccountSwitchError::PreparationFailed.to_string())
        })?,
    };
    if resolved.execution_account().binding != expected_binding {
        return Err(CodexErr::InvalidRequest(
            ExecutionAccountSwitchError::PreparationFailed.to_string(),
        ));
    }
    let prepared = session
        .prepare_execution_account_runtime(
            Arc::clone(resolved.execution_account()),
            resolved.services().clone(),
        )
        .await
        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
    Ok(AccountAttemptCandidate {
        binding: expected_binding,
        policy_revision: Some(policy_revision),
        binding_transition,
        resolved: Some(resolved),
        prepared: Some(prepared),
    })
}

async fn cleanup_prepared_runtime(prepared: PreparedExecutionAccountRuntime) {
    prepared.async_hook_results.close();
    prepared.hooks.shutdown().await;
    for runtime in &prepared.runtime.extension_runtimes {
        let _ = runtime.quiesce().await;
    }
    prepared.runtime.guardian_review_session.shutdown().await;
    prepared.runtime.mcp_runtime.shutdown().await;
}
