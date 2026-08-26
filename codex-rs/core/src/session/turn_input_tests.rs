use super::*;
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
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use codex_protocol::AgentPath;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::turn_input::TurnInput as SubmittedTurnInput;
use codex_protocol::user_input::UserInput;
use codex_thread_store::ThreadAccountRotationMode;
use codex_thread_store::ThreadAccountRotationPolicyUpdate;
use codex_thread_store::ThreadStore;
use core_test_support::test_codex::local_selections;
use pretty_assertions::assert_eq;
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
                    model: turn_context.model_info.slug.clone(),
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
        state.session_configuration.approvals_reviewer,
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
        (Vec::<TurnInput>::new(), None, None),
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
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
            .await,
        (Vec::<TurnInput>::new(), None, None)
    );

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn start_only_rejects_plan_mode_without_injecting() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let mut collaboration_mode = session.collaboration_mode().await;
    collaboration_mode.mode = ModeKind::Plan;
    {
        let mut state = session.state.lock().await;
        state.session_configuration.collaboration_mode = collaboration_mode;
    }

    let submission = submit_start_only(
        &session,
        SubmittedTurnInput::ResponseItem(user_message("synthetic idle input")),
    )
    .await;

    assert_eq!(
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PlanMode,
        },
        submission
    );
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(
        (Vec::<TurnInput>::new(), None, None),
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
    );
}

#[tokio::test]
async fn start_only_accepts_user_input_in_plan_mode() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let mut collaboration_mode = session.collaboration_mode().await;
    collaboration_mode.mode = ModeKind::Plan;
    {
        let mut state = session.state.lock().await;
        state.session_configuration.collaboration_mode = collaboration_mode;
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
        state.session_configuration.collaboration_mode = collaboration_mode;
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
            /*parent_turn_id*/ None,
            /*root_turn_id*/ None,
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
            .new_default_turn_with_sub_id("turn".to_string())
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
