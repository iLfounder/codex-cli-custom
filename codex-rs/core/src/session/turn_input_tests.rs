use super::*;
use crate::config::Constrained;
use crate::session::step_settings::StepSettingsUpdate;
use crate::execution_account::ExecutionAccountContext;
use crate::execution_account::ExecutionAccountServices;
use crate::execution_account::PreparedTurnExecutionAccountTransition;
use crate::execution_account::ResolvedExecutionAccountTransition;
use crate::execution_account::TurnExecutionAccountSelector;
use crate::execution_account::TurnExecutionAccountSelectorFuture;
use crate::execution_account::TurnExecutionAccountTransitionResolver;
use crate::execution_account::TurnExecutionAccountTransitionResolverFuture;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use codex_protocol::AgentPath;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::Settings;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::turn_input::TurnInput as SubmittedTurnInput;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::test_codex::local_selections;
use pretty_assertions::assert_eq;
use test_case::test_case;
use codex_thread_store::ThreadAccountRotationMode;
use codex_thread_store::ThreadAccountRotationPolicyUpdate;
use codex_thread_store::ThreadStore;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
struct NeverEndingTask {
    kind: TaskKind,
    listen_to_cancellation_token: bool,
}

struct StaticExecutionAccountSelector {
    decision: TurnExecutionAccountDecision,
}

struct BlockingExecutionAccountSelector {
    started: async_channel::Sender<()>,
}

struct RecordingExecutionAccountSelector {
    selected: async_channel::Sender<TurnExecutionAccountSelection>,
}

impl TurnExecutionAccountSelector for BlockingExecutionAccountSelector {
    fn select(
        &self,
        _selection: TurnExecutionAccountSelection,
    ) -> TurnExecutionAccountSelectorFuture<'_> {
        Box::pin(async move {
            self.started
                .send(())
                .await
                .expect("selector start observer remains open");
            std::future::pending().await
        })
    }
}

impl TurnExecutionAccountSelector for StaticExecutionAccountSelector {
    fn select(
        &self,
        _selection: TurnExecutionAccountSelection,
    ) -> TurnExecutionAccountSelectorFuture<'_> {
        let decision = self.decision.clone();
        Box::pin(async move { Ok(decision) })
    }
}

impl TurnExecutionAccountSelector for RecordingExecutionAccountSelector {
    fn select(
        &self,
        selection: TurnExecutionAccountSelection,
    ) -> TurnExecutionAccountSelectorFuture<'_> {
        Box::pin(async move {
            self.selected
                .send(selection)
                .await
                .expect("selection observer remains open");
            Ok(TurnExecutionAccountDecision::Keep)
        })
    }
}

struct TestReadinessLease {
    dropped: Arc<AtomicBool>,
}

impl Drop for TestReadinessLease {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

struct RecordingExecutionAccountTransitionResolver {
    target: Arc<ExecutionAccountContext>,
    services: ExecutionAccountServices,
    calls: Arc<AtomicUsize>,
    lease_dropped: Arc<AtomicBool>,
}

impl TurnExecutionAccountTransitionResolver for RecordingExecutionAccountTransitionResolver {
    fn resolve(
        &self,
        _current_binding: ExecutionAccountBinding,
        _target_slot_id: String,
    ) -> TurnExecutionAccountTransitionResolverFuture<'_> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let resolved = ResolvedExecutionAccountTransition::with_readiness_lease(
            Arc::clone(&self.target),
            TestReadinessLease {
                dropped: Arc::clone(&self.lease_dropped),
            },
        );
        let services = self.services.clone();
        Box::pin(async move {
            Ok(PreparedTurnExecutionAccountTransition::new(
                resolved, services,
            ))
        })
    }
}

async fn make_rotation_test_session() -> (
    Arc<Session>,
    Arc<codex_thread_store::InMemoryThreadStore>,
    ExecutionAccountBinding,
    u64,
) {
    let (mut session, _turn_context) = make_session_and_context().await;
    let store = attach_in_memory_thread_store(&mut session).await;
    let binding = session.execution_account().binding.clone();
    store
        .initialize_execution_account_binding(session.thread_id(), binding.clone())
        .await
        .expect("initialize execution account binding");
    let policy = store
        .compare_and_swap_thread_account_rotation_policy(
            session.thread_id(),
            /*expected_revision*/ 0,
            ThreadAccountRotationPolicyUpdate {
                mode: ThreadAccountRotationMode::RoundRobin,
                fixed_account_slot_id: None,
                automatic_account_slot_ids: vec![binding.slot_id.clone(), "target".to_string()],
            },
        )
        .await
        .expect("persist rotation policy")
        .expect("initial policy revision matches");
    (Arc::new(session), store, binding, policy.revision)
}

fn root_turn_request() -> TurnInputRequest {
    TurnInputRequest::user_input(vec![UserInput::Text {
        text: "rotate this turn".to_string(),
        text_elements: Vec::new(),
    }])
}

#[tokio::test]
async fn same_target_selection_skips_resolution_and_commits_cursor_after_start() {
    let (session, store, binding, policy_revision) = make_rotation_test_session().await;
    store
        .compare_and_swap_thread_account_rotation_cursor(
            session.thread_id(),
            policy_revision,
            "target".to_string(),
        )
        .await
        .expect("seed a distinguishable rotation cursor")
        .expect("policy revision matches");
    let calls = Arc::new(AtomicUsize::new(0));
    let lease_dropped = Arc::new(AtomicBool::new(false));
    let runtime = session.execution_account_runtime();
    session.set_turn_execution_account_selector(Arc::new(StaticExecutionAccountSelector {
        decision: TurnExecutionAccountDecision::Select {
            target_slot_id: binding.slot_id.clone(),
            policy_revision,
        },
    }));
    session.set_turn_execution_account_transition_resolver(Arc::new(
        RecordingExecutionAccountTransitionResolver {
            target: session.execution_account(),
            services: runtime.services.clone(),
            calls: Arc::clone(&calls),
            lease_dropped,
        },
    ));

    let submission = handle(
        &session,
        root_turn_request(),
        TurnInputMode::StartOrSteer,
        "same-target-turn".to_string(),
    )
    .await
    .expect("same-target selection starts");

    assert_eq!(
        submission,
        TurnInputSubmission::Started {
            turn_id: "same-target-turn".to_string(),
        }
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(session.execution_account().binding, binding.clone());
    let policy = store
        .thread_account_rotation_policy(session.thread_id())
        .await
        .expect("read rotation cursor");
    assert_eq!(policy.mode, ThreadAccountRotationMode::RoundRobin);
    assert_eq!(policy.last_committed_account_slot_id, Some(binding.slot_id));
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn selector_receives_each_same_slot_threads_exact_credential_revision() {
    let mut captured = Vec::new();
    for (index, api_key) in ["sk-thread-one", "sk-thread-two"].into_iter().enumerate() {
        let (session, _store, binding, _policy_revision) = make_rotation_test_session().await;
        let auth_home = tempfile::tempdir().expect("auth home");
        codex_login::login_with_api_key(
            auth_home.path(),
            api_key,
            codex_login::AuthCredentialsStoreMode::File,
            codex_login::AuthKeyringBackendKind::default(),
        )
        .expect("write test credentials");
        let auth_manager = Arc::new(
            codex_login::AuthManager::new(
                auth_home.path().to_path_buf(),
                /*enable_codex_api_key_env*/ false,
                codex_login::AuthCredentialsStoreMode::File,
                /*forced_chatgpt_workspace_id*/ None,
                /*chatgpt_base_url*/ None,
                codex_login::AuthKeyringBackendKind::default(),
                codex_login::test_support::transport_default_auth_route_config(),
            )
            .await,
        );
        let current = session.execution_account();
        let runtime = session.execution_account_runtime();
        let target = Arc::new(ExecutionAccountContext {
            binding: ExecutionAccountBinding {
                slot_id: binding.slot_id.clone(),
                generation: binding.generation + 1,
            },
            auth_manager: Arc::clone(&auth_manager),
            models_manager: Arc::clone(&current.models_manager),
        });
        let accepted = session
            .switch_execution_account(binding, Arc::clone(&target), runtime.services.clone())
            .await
            .expect("publish exact thread credential runtime");
        let (selected_tx, selected_rx) = async_channel::bounded(1);
        session.set_turn_execution_account_selector(Arc::new(RecordingExecutionAccountSelector {
            selected: selected_tx,
        }));

        let turn_id = format!("credential-revision-turn-{index}");
        let submission = handle(
            &session,
            root_turn_request(),
            TurnInputMode::StartIfIdle,
            turn_id.clone(),
        )
        .await
        .expect("start same-slot thread turn");
        let selection = selected_rx.recv().await.expect("capture selection");

        assert_eq!(
            (submission, selection.clone()),
            (
                TurnInputSubmission::Started { turn_id },
                TurnExecutionAccountSelection {
                    thread_id: session.thread_id(),
                    current_binding: accepted,
                    credential_revision: auth_manager.credential_revision(),
                },
            )
        );
        captured.push(selection);
        session.abort_all_tasks(TurnAbortReason::Interrupted).await;
    }

    assert_eq!(captured[0].current_binding, captured[1].current_binding);
    assert_ne!(
        captured[0].credential_revision,
        captured[1].credential_revision
    );
}

#[tokio::test]
async fn same_slot_reprepare_prepares_once_and_advances_execution_generation() {
    let (session, store, binding, policy_revision) = make_rotation_test_session().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let lease_dropped = Arc::new(AtomicBool::new(false));
    let current = session.execution_account();
    let runtime = session.execution_account_runtime();
    let target = Arc::new(ExecutionAccountContext {
        binding: ExecutionAccountBinding {
            slot_id: binding.slot_id.clone(),
            generation: binding.generation + 1,
        },
        auth_manager: Arc::clone(&current.auth_manager),
        models_manager: Arc::clone(&current.models_manager),
    });
    session.set_turn_execution_account_selector(Arc::new(StaticExecutionAccountSelector {
        decision: TurnExecutionAccountDecision::ReprepareCurrent { policy_revision },
    }));
    session.set_turn_execution_account_transition_resolver(Arc::new(
        RecordingExecutionAccountTransitionResolver {
            target: Arc::clone(&target),
            services: runtime.services.clone(),
            calls: Arc::clone(&calls),
            lease_dropped: Arc::clone(&lease_dropped),
        },
    ));

    let submission = handle(
        &session,
        root_turn_request(),
        TurnInputMode::StartIfIdle,
        "same-slot-reprepare-turn".to_string(),
    )
    .await
    .expect("same-slot reprepare starts");

    assert_eq!(
        (
            submission,
            calls.load(Ordering::Relaxed),
            lease_dropped.load(Ordering::Acquire),
            session.execution_account().binding.clone(),
        ),
        (
            TurnInputSubmission::Started {
                turn_id: "same-slot-reprepare-turn".to_string(),
            },
            1,
            true,
            target.binding.clone(),
        )
    );
    let policy = store
        .thread_account_rotation_policy(session.thread_id())
        .await
        .expect("read preserved rotation policy");
    assert_eq!(
        (
            policy.mode,
            policy.fixed_account_slot_id,
            policy.last_committed_account_slot_id,
        ),
        (
            ThreadAccountRotationMode::RoundRobin,
            None,
            Some(binding.slot_id),
        )
    );
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn stale_same_slot_reprepare_is_rechecked_once_without_starting() {
    let (session, store, binding, stale_policy_revision) = make_rotation_test_session().await;
    let current_policy = store
        .compare_and_swap_thread_account_rotation_policy(
            session.thread_id(),
            stale_policy_revision,
            ThreadAccountRotationPolicyUpdate {
                mode: ThreadAccountRotationMode::RoundRobin,
                fixed_account_slot_id: None,
                automatic_account_slot_ids: vec![binding.slot_id.clone()],
            },
        )
        .await
        .expect("update rotation policy")
        .expect("policy revision matches");
    let calls = Arc::new(AtomicUsize::new(0));
    let current = session.execution_account();
    let runtime = session.execution_account_runtime();
    session.set_turn_execution_account_selector(Arc::new(StaticExecutionAccountSelector {
        decision: TurnExecutionAccountDecision::ReprepareCurrent {
            policy_revision: stale_policy_revision,
        },
    }));
    session.set_turn_execution_account_transition_resolver(Arc::new(
        RecordingExecutionAccountTransitionResolver {
            target: Arc::new(ExecutionAccountContext {
                binding: ExecutionAccountBinding {
                    slot_id: binding.slot_id.clone(),
                    generation: binding.generation + 1,
                },
                auth_manager: Arc::clone(&current.auth_manager),
                models_manager: Arc::clone(&current.models_manager),
            }),
            services: runtime.services.clone(),
            calls: Arc::clone(&calls),
            lease_dropped: Arc::new(AtomicBool::new(false)),
        },
    ));

    let error = handle(
        &session,
        root_turn_request(),
        TurnInputMode::StartIfIdle,
        "stale-same-slot-reprepare".to_string(),
    )
    .await
    .expect_err("stale policy cannot start");

    let active_turn_is_none = session.active_turn.lock().await.is_none();
    let unchanged_policy = store
        .thread_account_rotation_policy(session.thread_id())
        .await
        .expect("read unchanged current policy");
    assert_eq!(
        (
            error.to_string(),
            calls.load(Ordering::Relaxed),
            session.execution_account().binding.clone(),
            active_turn_is_none,
            unchanged_policy,
        ),
        (
            ExecutionAccountSwitchError::StaleGeneration.to_string(),
            2,
            binding,
            true,
            current_policy,
        )
    );
}

#[tokio::test]
async fn different_target_selection_preserves_rotation_and_holds_readiness_through_switch() {
    let (session, store, binding, policy_revision) = make_rotation_test_session().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let lease_dropped = Arc::new(AtomicBool::new(false));
    let current = session.execution_account();
    let runtime = session.execution_account_runtime();
    let target = Arc::new(ExecutionAccountContext {
        binding: ExecutionAccountBinding {
            slot_id: "target".to_string(),
            generation: binding.generation + 1,
        },
        auth_manager: Arc::clone(&current.auth_manager),
        models_manager: Arc::clone(&current.models_manager),
    });
    session.set_turn_execution_account_selector(Arc::new(StaticExecutionAccountSelector {
        decision: TurnExecutionAccountDecision::Select {
            target_slot_id: "target".to_string(),
            policy_revision,
        },
    }));
    session.set_turn_execution_account_transition_resolver(Arc::new(
        RecordingExecutionAccountTransitionResolver {
            target: Arc::clone(&target),
            services: runtime.services.clone(),
            calls: Arc::clone(&calls),
            lease_dropped: Arc::clone(&lease_dropped),
        },
    ));

    let submission = handle(
        &session,
        root_turn_request(),
        TurnInputMode::StartIfIdle,
        "different-target-turn".to_string(),
    )
    .await
    .expect("different-target selection starts");

    assert_eq!(
        submission,
        TurnInputSubmission::Started {
            turn_id: "different-target-turn".to_string(),
        }
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(lease_dropped.load(Ordering::Acquire));
    assert_eq!(session.execution_account().binding, target.binding);
    let policy = store
        .thread_account_rotation_policy(session.thread_id())
        .await
        .expect("read preserved rotation policy");
    assert_eq!(
        (
            policy.mode,
            policy.fixed_account_slot_id,
            policy.last_committed_account_slot_id,
        ),
        (
            ThreadAccountRotationMode::RoundRobin,
            None,
            Some("target".to_string()),
        )
    );
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn cancelling_account_selection_releases_exact_idle_reservation_without_cursor_commit() {
    let (session, store, binding, _policy_revision) = make_rotation_test_session().await;
    let initial_policy = store
        .thread_account_rotation_policy(session.thread_id())
        .await
        .expect("read initial policy");
    let (started_tx, started_rx) = async_channel::bounded(1);
    session.set_turn_execution_account_selector(Arc::new(BlockingExecutionAccountSelector {
        started: started_tx,
    }));
    let submission = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            handle(
                &session,
                root_turn_request(),
                TurnInputMode::StartIfIdle,
                "cancelled-selection".to_string(),
            )
            .await
        }
    });
    started_rx.recv().await.expect("selector started");

    session.cancel_execution_account_preparation();

    let error = submission
        .await
        .expect("submission task joins")
        .expect_err("cancelled selection cannot start");
    assert_eq!(
        error.to_string(),
        ExecutionAccountSwitchError::ThreadBusy.to_string()
    );
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(session.execution_account().binding, binding);
    let policy = store
        .thread_account_rotation_policy(session.thread_id())
        .await
        .expect("read unchanged policy");
    assert_eq!(policy, initial_policy);
}

impl SessionTask for NeverEndingTask {
    fn kind(&self) -> TaskKind {
        self.kind
    }

    fn span_name(&self) -> &'static str {
        "session_task.turn_input_test"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        if self.listen_to_cancellation_token {
            cancellation_token.cancelled().await;
            return Ok(None);
        }
        loop {
            sleep(std::time::Duration::from_secs(60)).await;
        }
    }
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[tokio::test]
async fn account_mismatch_rejects_start_and_steer_before_input_handling() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let actual = session.execution_account().binding.clone();
    let expected = ExecutionAccountBinding {
        slot_id: actual.slot_id.clone(),
        generation: actual.generation.wrapping_add(1),
    };
    let expected_submission = TurnInputSubmission::NotSubmitted {
        reason: NotSubmittedReason::ExpectedExecutionAccountMismatch {
            expected: expected.clone(),
            actual,
        },
    };

    for mode in [
        TurnInputMode::StartIfIdle,
        TurnInputMode::Steer {
            expected_turn_id: "missing-turn".to_string(),
        },
    ] {
        let submission = handle(
            &session,
            TurnInputRequest::new(SubmittedTurnInput::ResponseItem(user_message(
                "stale prompt",
            )))
            .with_expected_execution_account(expected.clone()),
            mode,
            "account-fenced-submission".to_string(),
        )
        .await
        .expect("account mismatch should be a submission result");

        assert_eq!(submission, expected_submission);
    }
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "the test proves turn admission waits on the transition fence"
)]
async fn turn_and_mailbox_admission_wait_for_execution_control_transition() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let transition = session.execution_runtime_transition_lock.lock().await;
    let session_for_submission = Arc::clone(&session);
    let mut submission = tokio::spawn(async move {
        handle(
            &session_for_submission,
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "queued while closing".to_string(),
                text_elements: Vec::new(),
            }]),
            TurnInputMode::Steer {
                expected_turn_id: "missing-turn".to_string(),
            },
            "serialized-submission".to_string(),
        )
        .await
    });
    let session_for_delivery = Arc::clone(&session);
    let mut delivery = tokio::spawn(async move {
        crate::session::handlers::inter_agent_communication(
            &session_for_delivery,
            "mailbox-submission".to_string(),
            InterAgentCommunication::new(
                AgentPath::root(),
                AgentPath::root(),
                Vec::new(),
                "queued while closing".to_string(),
                /*trigger_turn*/ false,
            ),
            /*parent_turn_id*/ None,
            /*root_turn_id*/ None,
        )
        .await;
    });

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut submission)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut delivery)
            .await
            .is_err()
    );
    assert!(!session.input_queue.has_pending_mailbox_items().await);
    drop(transition);
    assert_eq!(
        submission
            .await
            .expect("submission task")
            .expect("submission"),
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NoActiveTurn,
        }
    );
    delivery.await.expect("mailbox delivery");
    assert!(session.input_queue.has_pending_mailbox_items().await);
}

async fn submit_start_only(
    session: &Arc<Session>,
    input: SubmittedTurnInput,
) -> TurnInputSubmission {
    handle(
        session,
        TurnInputRequest::new(input),
        TurnInputMode::StartIfIdle,
        "test-submission".to_string(),
    )
    .await
    .expect("start-only submission should be valid")
}

async fn submit_steer_only(
    session: &Arc<Session>,
    input: Vec<UserInput>,
    expected_turn_id: &str,
) -> TurnInputSubmission {
    handle(
        session,
        TurnInputRequest::new(SubmittedTurnInput::UserInput {
            content: input,
            client_id: None,
        }),
        TurnInputMode::Steer {
            expected_turn_id: expected_turn_id.to_string(),
        },
        "test-submission".to_string(),
    )
    .await
    .expect("steer-only submission should be valid")
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "simulate an in-flight realtime append while checking input admission"
)]
async fn steering_does_not_wait_for_realtime_history() {
    let (mut session, turn_context) = make_session_and_context().await;
    session.realtime_history = Some(tokio::sync::Mutex::new(Default::default()));
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;

    let history = session
        .realtime_history
        .as_ref()
        .expect("realtime history")
        .lock()
        .await;
    for mode in [
        TurnInputMode::StartOrSteer,
        TurnInputMode::Steer {
            expected_turn_id: turn_context.sub_id.clone(),
        },
    ] {
        let submission = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle(
                &session,
                TurnInputRequest::user_input(vec![UserInput::Text {
                    text: "steer without waiting for persistence".to_string(),
                    text_elements: Vec::new(),
                }]),
                mode,
                "steer-submission".to_string(),
            ),
        )
        .await
        .expect("steering must not wait for the realtime recorder")
        .expect("steering should succeed");
        assert_eq!(
            submission,
            TurnInputSubmission::Steered {
                turn_id: turn_context.sub_id.clone()
            }
        );
    }
    drop(history);
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn accepted_input_applies_thread_settings() {
    let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
    let config = session.get_config().await;
    handle(
        &session,
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }])
        .with_thread_settings(ThreadSettingsOverrides {
            environments: Some(local_selections(config.cwd.clone())),
            approval_policy: Some(config.permissions.approval_policy.value()),
            approvals_reviewer: Some(codex_config::types::ApprovalsReviewer::AutoReview),
            sandbox_policy: Some(config.legacy_sandbox_policy()),
            summary: config.model_reasoning_summary,
            personality: config.personality,
            collaboration_mode: Some(CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: turn_context.model_info().slug.clone(),
                    reasoning_effort: config.model_reasoning_effort.clone(),
                    developer_instructions: None,
                },
            }),
            ..Default::default()
        }),
        TurnInputMode::StartOrSteer,
        "sub-1".to_string(),
    )
    .await
    .expect("submit user turn");

    let state = session.state.lock().await;
    assert_eq!(
        state.session_configuration.step_settings.approvals_reviewer,
        codex_config::types::ApprovalsReviewer::AutoReview
    );
    assert!(
        session.mcp_refresh.is_pending(),
        "server elicitation authority changes must refresh MCP state"
    );
}

#[tokio::test]
async fn start_only_rejects_active_turn_without_injecting() {
    let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;

    let input = SubmittedTurnInput::ResponseItem(user_message("synthetic idle input"));
    let submission = submit_start_only(&session, input).await;

    assert_eq!(
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NotIdle,
        },
        submission
    );
    assert_eq!(
        Vec::<TurnInput>::new(),
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .0
    );

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn recovery_rejects_active_turn_without_injecting_or_applying_settings() {
    let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
    let original_approval_policy = session
        .get_config()
        .await
        .permissions
        .approval_policy
        .value();
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;

    let submission = handle_recovery(
        &session,
        ThreadSettingsOverrides {
            approval_policy: Some(AskForApproval::Never),
            ..Default::default()
        },
        TurnStartOptions::default(),
        "recovered-turn".to_string(),
    )
    .await
    .expect("recovery should return a typed rejection");

    assert_eq!(
        submission,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NotIdle,
        }
    );
    assert_eq!(
        session
            .get_config()
            .await
            .permissions
            .approval_policy
            .value(),
        original_approval_policy
    );
    assert_eq!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .0,
        Vec::<TurnInput>::new()
    );

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn start_only_rejects_current_plan_before_validating_settings() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let default_mode = session.collaboration_mode().await;
    {
        let mut state = session.state.lock().await;
        let settings = Arc::make_mut(&mut state.session_configuration.step_settings);
        settings.collaboration_mode.mode = ModeKind::Plan;
        settings.approval_policy = Constrained::allow_only(AskForApproval::OnRequest);
    }
    let desired_settings = session.thread_settings_snapshot().await;
    let invalid_override = ThreadSettingsOverrides {
        collaboration_mode: Some(default_mode.clone()),
        approval_policy: Some(AskForApproval::Never),
        ..Default::default()
    };

    // Current Plan takes precedence even when the request would leave Plan or
    // fail settings validation. Nothing has been reserved or applied yet.
    let submission = handle(
        &session,
        TurnInputRequest::new(SubmittedTurnInput::ResponseItem(user_message(
            "synthetic idle input",
        )))
        .with_thread_settings(invalid_override.clone()),
        TurnInputMode::StartIfIdle,
        "automatic-plan-submission".to_string(),
    )
    .await
    .expect("current Plan must reject before settings validation");
    assert_eq!(
        submission,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PlanMode,
        }
    );
    assert_eq!(session.thread_settings_snapshot().await, desired_settings);
    assert!(session.active_turn.lock().await.is_none());

    session
        .update_settings(SessionSettingsUpdate {
            step_settings: StepSettingsUpdate {
                collaboration_mode: Some(default_mode),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("explicit settings may leave Plan mode");
    let desired_settings = session.thread_settings_snapshot().await;
    let result = handle(
        &session,
        TurnInputRequest::new(SubmittedTurnInput::ResponseItem(user_message(
            "invalid automatic input",
        )))
        .with_thread_settings(invalid_override),
        TurnInputMode::StartIfIdle,
        "invalid-automatic-submission".to_string(),
    )
    .await;
    let error = result.expect_err("invalid automatic settings must be rejected");
    assert!(matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(_)
    ));
    assert_eq!(session.thread_settings_snapshot().await, desired_settings);
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(
        Vec::<TurnInput>::new(),
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .0
    );
}

#[tokio::test]
async fn prepared_user_updates_merge_with_settings_at_turn_start() {
    for (requested, intervening) in [
        (
            ThreadSettingsOverrides {
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
            ThreadSettingsOverrides {
                model: Some("gpt-5.2".to_string()),
                ..Default::default()
            },
        ),
        (
            ThreadSettingsOverrides {
                model: Some("gpt-5.2".to_string()),
                ..Default::default()
            },
            ThreadSettingsOverrides {
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
        ),
    ] {
        let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
        let initial = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model: "gpt-5.4".to_string(),
                reasoning_effort: Some(ReasoningEffort::Low),
                developer_instructions: None,
            },
        };
        session
            .update_settings(thread_settings::prepare_update(ThreadSettingsOverrides {
                collaboration_mode: Some(initial.clone()),
                ..Default::default()
            }))
            .await
            .expect("set initial model and effort");
        let prepared =
            PreparedTurnInputSettings::prepare(&session, requested, TurnStartOptions::default())
                .await
                .expect("prepare partial protocol update");
        session
            .update_settings(thread_settings::prepare_update(intervening))
            .await
            .expect("commit intervening settings");

        let turn_context = prepared
            .apply_started(
                &session,
                "sparse-user-start".to_string(),
                TurnStartKind::User,
            )
            .await
            .expect("apply prepared settings")
            .expect("user start is permitted");
        let expected = initial.with_updates(
            Some("gpt-5.2".to_string()),
            Some(Some(ReasoningEffort::High)),
            /*developer_instructions*/ None,
        );
        assert_eq!(session.collaboration_mode().await, expected);
        assert_eq!(turn_context.collaboration_mode(), expected);
        assert_eq!(
            turn_context.initial_settings.selected_collaboration_mode(),
            &expected
        );
        assert!(Arc::ptr_eq(
            &turn_context.initial_settings.model_info,
            turn_context.model_info(),
        ));
    }
}

#[tokio::test]
async fn automatic_admission_uses_current_candidate_after_plan_preview() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let default_mode = session.collaboration_mode().await;
    let mut plan_mode = default_mode.clone();
    plan_mode.mode = ModeKind::Plan;
    session
        .update_settings(thread_settings::prepare_update(ThreadSettingsOverrides {
            collaboration_mode: Some(plan_mode),
            ..Default::default()
        }))
        .await
        .expect("enter Plan after the initial admission check");
    let prepared = PreparedTurnInputSettings::prepare(
        &session,
        ThreadSettingsOverrides {
            effort: Some(Some(ReasoningEffort::High)),
            ..Default::default()
        },
        TurnStartOptions::default(),
    )
    .await
    .expect("validate the patch while the preview is Plan");
    session
        .update_settings(thread_settings::prepare_update(ThreadSettingsOverrides {
            collaboration_mode: Some(default_mode.clone()),
            ..Default::default()
        }))
        .await
        .expect("leave Plan before atomic admission");

    let turn_context = prepared
        .apply_started(
            &session,
            "automatic-after-plan-preview".to_string(),
            TurnStartKind::Automatic,
        )
        .await
        .expect("automatic admission should succeed")
        .expect("current and proposed modes are both Default");
    let expected = default_mode.with_updates(
        /*model*/ None,
        Some(Some(ReasoningEffort::High)),
        /*developer_instructions*/ None,
    );
    assert_eq!(session.collaboration_mode().await, expected);
    assert_eq!(turn_context.collaboration_mode(), expected);
    assert_eq!(
        turn_context.initial_settings.selected_collaboration_mode(),
        &expected
    );
}

#[tokio::test]
async fn automatic_admission_rechecks_plan_mode_without_committing_sparse_settings() {
    struct ConfigRecorder(Arc<std::sync::Mutex<Vec<(AskForApproval, ApprovalsReviewer)>>>);

    impl codex_extension_api::ConfigContributor<crate::config::Config> for ConfigRecorder {
        fn on_config_changed(
            &self,
            _session_store: &codex_extension_api::ExtensionData,
            _thread_store: &codex_extension_api::ExtensionData,
            _previous_config: &crate::config::Config,
            new_config: &crate::config::Config,
        ) {
            self.0.lock().expect("config records lock").push((
                new_config.permissions.approval_policy.value(),
                new_config.approvals_reviewer,
            ));
        }
    }

    let (mut session, _turn_context, rx) = make_session_and_context_with_rx().await;
    let records = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    extensions.config_contributor(Arc::new(ConfigRecorder(Arc::clone(&records))));
    Arc::get_mut(&mut session)
        .expect("unique test session")
        .services
        .extensions = Arc::new(extensions.build());
    {
        let mut state = session.state.lock().await;
        let settings = Arc::make_mut(&mut state.session_configuration.step_settings);
        settings.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
        settings.approvals_reviewer = ApprovalsReviewer::User;
    }
    let original_environments = session.services.turn_environments.selections();
    let workspace = tempfile::tempdir().expect("create proposed workspace");
    let proposed_environments = local_selections(
        AbsolutePathBuf::try_from(workspace.path()).expect("absolute workspace path"),
    );
    assert_ne!(original_environments, proposed_environments.environments);
    let default_mode = session.collaboration_mode().await;
    let overrides = ThreadSettingsOverrides {
        model: Some("automatic-model-must-not-be-applied".to_string()),
        service_tier: Some(Some(ServiceTier::Fast.request_value().to_string())),
        environments: Some(proposed_environments.clone()),
        approval_policy: Some(AskForApproval::Never),
        approvals_reviewer: Some(ApprovalsReviewer::AutoReview),
        ..Default::default()
    };
    let default_override = ThreadSettingsOverrides {
        collaboration_mode: Some(default_mode.clone()),
        ..overrides.clone()
    };
    let prepared = PreparedTurnInputSettings::prepare(
        &session,
        overrides.clone(),
        TurnStartOptions::default(),
    )
    .await
    .expect("sparse settings should preview successfully");
    let prepared_default =
        PreparedTurnInputSettings::prepare(&session, default_override, TurnStartOptions::default())
            .await
            .expect("Default replacement should preview successfully");

    // Another settings writer changes the effective mode after preview. The
    // sparse patch must not commit in Plan, and a full Default replacement
    // must not let automatic work escape the now-current Plan configuration.
    let mut collaboration_mode = default_mode;
    collaboration_mode.mode = ModeKind::Plan;
    session
        .update_settings(SessionSettingsUpdate {
            step_settings: StepSettingsUpdate {
                collaboration_mode: Some(collaboration_mode),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("Plan mode should be allowed for explicit settings updates");
    let desired_settings = session.thread_settings_snapshot().await;
    records.lock().expect("config records lock").clear();
    // Keep the existing refresh worker from consuming invalidation while the
    // rejection and its positive control inspect MCP's dirty state.
    let _mcp_refresh = session
        .mcp_refresh
        .acquire()
        .await
        .expect("acquire MCP refresh gate");
    session.mcp_refresh.claim();
    assert!(!session.mcp_refresh.is_pending());

    for (submission_id, prepared) in [
        ("automatic-after-preview", prepared),
        ("automatic-default-after-preview", prepared_default),
    ] {
        let outcome = prepared
            .apply_started(
                &session,
                submission_id.to_string(),
                TurnStartKind::Automatic,
            )
            .await
            .expect("automatic admission should return a typed rejection");
        assert!(outcome.is_none());
        assert_eq!(session.thread_settings_snapshot().await, desired_settings);
        assert!(session.active_turn.lock().await.is_none());
        assert_eq!(
            session.services.turn_environments.selections(),
            original_environments
        );
        assert!(!session.mcp_refresh.is_pending());
        assert_eq!(*records.lock().expect("config records lock"), Vec::new());
        while let Ok(event) = rx.try_recv() {
            assert_ne!(event.id, submission_id);
        }
    }

    assert_eq!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .0,
        Vec::<TurnInput>::new()
    );

    // The rejected candidate is valid and would have real runtime effects if
    // accepted by an ordinary settings update.
    session
        .update_settings(thread_settings::prepare_update(overrides))
        .await
        .expect("explicit settings update accepts the same patch");
    assert_eq!(
        session.services.turn_environments.selections(),
        proposed_environments.environments
    );
    assert!(session.mcp_refresh.is_pending());
    assert_eq!(
        *records.lock().expect("config records lock"),
        vec![(AskForApproval::Never, ApprovalsReviewer::AutoReview)]
    );
}

#[test_case(TurnStartKind::User; "ordinary constructor")]
#[test_case(TurnStartKind::Automatic; "conditional constructor")]
#[tokio::test]
async fn admission_revalidates_constraints_before_committing(kind: TurnStartKind) {
    let (session, _turn_context, rx) = make_session_and_context_with_rx().await;
    {
        let mut state = session.state.lock().await;
        Arc::make_mut(&mut state.session_configuration.step_settings).approval_policy =
            Constrained::allow_any(AskForApproval::OnRequest);
    }
    let prepared = PreparedTurnInputSettings::prepare(
        &session,
        ThreadSettingsOverrides {
            approval_policy: Some(AskForApproval::Never),
            service_tier: Some(Some(ServiceTier::Fast.request_value().to_string())),
            ..Default::default()
        },
        TurnStartOptions::default(),
    )
    .await
    .expect("approval-policy edit should initially be valid");

    let approval_policy = Constrained::allow_only(AskForApproval::OnRequest);
    let expected_message = CodexErr::InvalidRequest(
        approval_policy
            .can_set(&AskForApproval::Never)
            .expect_err("new constraint must reject the prepared edit")
            .to_string(),
    )
    .to_string();
    {
        let mut state = session.state.lock().await;
        Arc::make_mut(&mut state.session_configuration.step_settings).approval_policy =
            approval_policy;
    }
    let desired_settings = session.thread_settings_snapshot().await;
    let submission_id = "constraints-after-preview";
    let result = prepared
        .apply_started(&session, submission_id.to_string(), kind)
        .await;
    let Err(error) = result else {
        panic!("commit-time constraint failure must return InvalidRequest");
    };
    let CodexErrorDetails::InvalidRequest(message) = error.details() else {
        panic!("unexpected commit-time error: {error}");
    };
    assert_eq!(message, &expected_message);
    let errors: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|event| event.id == submission_id)
        .map(|event| match event.msg {
            EventMsg::Error(error) => error,
            other => panic!("unexpected rejected-turn event: {other:?}"),
        })
        .collect();
    assert_eq!(
        errors,
        vec![ErrorEvent {
            misalignment: None,
            message: expected_message,
            codex_error_info: Some(CodexErrorInfo::BadRequest),
        }]
    );
    assert_eq!(session.thread_settings_snapshot().await, desired_settings);
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .0,
        Vec::<TurnInput>::new()
    );
}

#[tokio::test]
async fn start_only_accepts_user_input_in_plan_mode() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let mut collaboration_mode = session.collaboration_mode().await;
    collaboration_mode.mode = ModeKind::Plan;
    {
        let mut state = session.state.lock().await;
        Arc::make_mut(&mut state.session_configuration.step_settings).collaboration_mode =
            collaboration_mode;
        state.merge_connector_selection(["calendar".to_string()]);
    }

    let submission = submit_start_only(
        &session,
        SubmittedTurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "queued user input".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: Some("queued-user-message".to_string()),
        },
    )
    .await;
    assert!(matches!(submission, TurnInputSubmission::Started { .. }));
    assert!(
        session
            .state
            .lock()
            .await
            .get_connector_selection()
            .is_empty()
    );

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn start_only_rejects_empty_user_input_in_plan_mode() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let mut collaboration_mode = session.collaboration_mode().await;
    collaboration_mode.mode = ModeKind::Plan;
    {
        let mut state = session.state.lock().await;
        Arc::make_mut(&mut state.session_configuration.step_settings).collaboration_mode =
            collaboration_mode;
    }

    let submission = submit_start_only(
        &session,
        SubmittedTurnInput::UserInput {
            content: Vec::new(),
            client_id: Some("empty-queued-user-message".to_string()),
        },
    )
    .await;

    assert_eq!(
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PlanMode,
        },
        submission
    );
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn start_only_rejects_pending_trigger_turn_without_injecting() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    {
        let mut state = session.state.lock().await;
        Arc::make_mut(&mut state.session_configuration.step_settings)
            .collaboration_mode
            .mode = ModeKind::Plan;
    }
    session
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::root(),
                AgentPath::root(),
                Vec::new(),
                "pending trigger".to_string(),
                /*trigger_turn*/ true,
            ),
            Default::default(),
        )
        .await;

    let submission = submit_start_only(
        &session,
        SubmittedTurnInput::ResponseItem(user_message("synthetic idle input")),
    )
    .await;

    assert_eq!(
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PendingTriggerTurn,
        },
        submission
    );
    assert!(session.active_turn.lock().await.is_none());
    assert!(session.input_queue.has_trigger_turn_mailbox_items().await);
    assert_eq!(session.collaboration_mode().await.mode, ModeKind::Plan);
}

#[tokio::test]
async fn steer_only_requires_active_turn() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let submission = submit_steer_only(
        &session,
        vec![UserInput::Text {
            text: "steer".to_string(),
            text_elements: Vec::new(),
        }],
        "missing-turn-id",
    )
    .await;

    assert_eq!(
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NoActiveTurn,
        },
        submission
    );
}

#[tokio::test]
async fn steer_only_enforces_expected_turn_id() {
    let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
    session
        .spawn_task(
            Arc::clone(&turn_context),
            vec![TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            }],
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;

    let submission = submit_steer_only(
        &session,
        vec![UserInput::Text {
            text: "steer".to_string(),
            text_elements: Vec::new(),
        }],
        "different-turn-id",
    )
    .await;
    assert_eq!(
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::ExpectedTurnMismatch {
                expected: "different-turn-id".to_string(),
                actual: turn_context.sub_id.clone(),
            },
        },
        submission
    );

    let output: ResponseItem = serde_json::from_value(serde_json::json!({
        "type": "function_call_output",
        "name": "send_message_to_thread",
        "output": "delegated work",
    }))
    .expect("valid standalone output");

    let submission = handle(
        &session,
        TurnInputRequest::new(SubmittedTurnInput::ResponseItem(output)),
        TurnInputMode::StartOrSteer,
        "test-submission".to_string(),
    )
    .await
    .expect("standalone output should steer the active turn");

    assert_eq!(
        submission,
        TurnInputSubmission::Steered {
            turn_id: turn_context.sub_id.clone()
        }
    );
    let turn_state = session
        .input_queue
        .turn_state_for_sub_id(&session.active_turn, &turn_context.sub_id)
        .await
        .expect("active turn state");
    assert_eq!(
        session
            .input_queue
            .subscribe_activity(Some(turn_state.as_ref()))
            .await
            .1,
        Some(crate::session::input_queue::InputQueueActivity::Steer)
    );
}

#[tokio::test]
async fn rejects_non_regular_turns() {
    for (task_kind, turn_kind) in [
        (TaskKind::Review, NonSteerableTurnKind::Review),
        (TaskKind::Compact, NonSteerableTurnKind::Compact),
    ] {
        let (session, incoming_turn_context, _rx) = make_session_and_context_with_rx().await;
        incoming_turn_context
            .turn_metadata_state
            .set_root_turn_id("incoming-root".to_string());
        let turn_context = session
            .new_turn_with_default_settings("turn".to_string(), Default::default())
            .await;
        turn_context
            .turn_metadata_state
            .set_root_turn_id("active-root".to_string());
        session
            .spawn_task(
                Arc::clone(&turn_context),
                vec![TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "hello".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
                NeverEndingTask {
                    kind: task_kind,
                    listen_to_cancellation_token: true,
                },
            )
            .await;

        let steer_input = vec![UserInput::Text {
            text: "steer".to_string(),
            text_elements: Vec::new(),
        }];
        let steer_submission = submit_steer_only(&session, steer_input.clone(), "turn").await;
        assert_eq!(
            TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::ActiveTurnNotSteerable { turn_kind },
            },
            steer_submission
        );
        let start_or_steer_submission = handle(
            &session,
            TurnInputRequest::user_input(steer_input),
            TurnInputMode::StartOrSteer,
            "test-submission".to_string(),
        )
        .await
        .expect("start-or-steer submission should be valid");
        assert_eq!(
            TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::ActiveTurnNotSteerable { turn_kind },
            },
            start_or_steer_submission
        );
        assert_eq!(
            turn_context.turn_metadata_state.root_turn_id().as_deref(),
            Some("active-root")
        );

        session.abort_all_tasks(TurnAbortReason::Interrupted).await;
    }
}
