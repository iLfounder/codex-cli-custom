use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_core::execution_account::ExecutionAccountContext;
use codex_core::execution_account::ExecutionAccountResolver;
use codex_core::execution_account::ExecutionAccountResolverFuture;
use codex_core::execution_account::SuccessfulAccountBindingTransition;
use codex_core::execution_account::TurnExecutionAccountDecision;
use codex_core::execution_account::TurnExecutionAccountFailoverSelection;
use codex_core::execution_account::TurnExecutionAccountSelection;
use codex_core::execution_account::TurnExecutionAccountSelector;
use codex_core::execution_account::TurnExecutionAccountSelectorFuture;
use codex_core::execution_account::TurnExecutionAccountSuccessCommit;
use codex_core::execution_account::TurnExecutionAccountSuccessCommitFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::error::AccountRejectionKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_thread_store::ThreadAccountRotationPolicy;
use core_test_support::responses;
use core_test_support::responses::mount_response_sequence;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::TempDir;
use tokio::sync::Notify;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

const SLOT_A: &str = "slot-a";
const SLOT_B: &str = "slot-b";

struct TwoAccountResolver {
    accounts: BTreeMap<String, (Arc<AuthManager>, SharedModelsManager)>,
}

impl ExecutionAccountResolver for TwoAccountResolver {
    fn initial_binding_for_new_thread(&self) -> ExecutionAccountBinding {
        ExecutionAccountBinding {
            slot_id: SLOT_A.to_string(),
            generation: 1,
        }
    }

    fn resolve(&self, binding: ExecutionAccountBinding) -> ExecutionAccountResolverFuture<'_> {
        let account = self.accounts.get(&binding.slot_id).cloned();
        Box::pin(async move {
            let (auth_manager, models_manager) = account.ok_or_else(|| {
                codex_protocol::error::CodexErr::InvalidRequest(
                    "test account is unavailable".to_string(),
                )
            })?;
            Ok(Arc::new(ExecutionAccountContext {
                binding,
                auth_manager,
                models_manager,
            }))
        })
    }
}

#[derive(Default)]
struct SelectorEvidence {
    failover: Vec<TurnExecutionAccountFailoverSelection>,
    commits: Vec<TurnExecutionAccountSuccessCommit>,
}

struct FailOnceSelector {
    evidence: Mutex<SelectorEvidence>,
}

impl TurnExecutionAccountSelector for FailOnceSelector {
    fn select(
        &self,
        _selection: TurnExecutionAccountSelection,
    ) -> TurnExecutionAccountSelectorFuture<'_> {
        Box::pin(async { Ok(TurnExecutionAccountDecision::Keep) })
    }

    fn pre_semantic_failover_enabled(&self) -> bool {
        true
    }

    fn select_failover(
        &self,
        selection: TurnExecutionAccountFailoverSelection,
    ) -> TurnExecutionAccountSelectorFuture<'_> {
        self.evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failover
            .push(selection);
        Box::pin(async {
            Ok(TurnExecutionAccountDecision::Select {
                target_slot_id: SLOT_B.to_string(),
            })
        })
    }

    fn commit_successful_selection(
        &self,
        commit: TurnExecutionAccountSuccessCommit,
    ) -> TurnExecutionAccountSuccessCommitFuture<'_> {
        let accepted = ExecutionAccountBinding {
            slot_id: commit.target_slot_id.clone(),
            generation: commit.expected_binding.generation + 1,
        };
        self.evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commits
            .push(commit);
        Box::pin(async move { Ok(accepted) })
    }
}

enum RuntimePreparation {
    Ready,
    Block(Arc<Notify>),
}

struct BlockingRuntimePreparation {
    started: Arc<Notify>,
}

impl codex_extension_api::ExecutionAccountRuntimeContributor<Config>
    for BlockingRuntimePreparation
{
    fn prepare<'a>(
        &'a self,
        _input: codex_extension_api::ExecutionAccountRuntimePrepareInput<'a, Config>,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Result<Arc<dyn codex_extension_api::PreparedExecutionAccountRuntime>, String>,
    > {
        self.started.notify_one();
        Box::pin(std::future::pending())
    }
}

async fn build_two_account_test(
    server: &MockServer,
    selector: Arc<FailOnceSelector>,
    runtime_preparation: RuntimePreparation,
) -> Result<TestCodex> {
    let home = Arc::new(TempDir::new()?);
    let provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers(/*openai_base_url*/ None)["openai"].clone()
    };
    let auth_a = codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key(
        "account-a-test-key",
    ));
    let auth_b = codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key(
        "account-b-test-key",
    ));
    let models_a = codex_core::test_support::models_manager_with_provider(
        home.path().to_path_buf(),
        Arc::clone(&auth_a),
        provider.clone(),
    );
    let models_b = codex_core::test_support::models_manager_with_provider(
        home.path().to_path_buf(),
        Arc::clone(&auth_b),
        provider,
    );
    let resolver = Arc::new(TwoAccountResolver {
        accounts: BTreeMap::from([
            (SLOT_A.to_string(), (auth_a, models_a.clone())),
            (SLOT_B.to_string(), (auth_b, models_b)),
        ]),
    });
    let mut builder = test_codex()
        .with_home(Arc::clone(&home))
        .with_models_manager(models_a)
        .with_execution_account_resolver(resolver)
        .with_turn_execution_account_selector(selector);
    if let RuntimePreparation::Block(started) = runtime_preparation {
        let mut extensions = ExtensionRegistryBuilder::<Config>::new();
        extensions.execution_account_runtime_contributor(Arc::new(BlockingRuntimePreparation {
            started,
        }));
        builder = builder.with_extensions(Arc::new(extensions.build()));
    }
    builder.build_with_auto_env(server).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_no_effect_rejection_fails_over_within_one_root_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let selector = Arc::new(FailOnceSelector {
        evidence: Mutex::new(SelectorEvidence::default()),
    });
    let response_mock = mount_response_sequence(
        &server,
        vec![
            ResponseTemplate::new(429).set_body_json(json!({
                "error": {
                    "type": "usage_limit_reached",
                    "message": "limit reached",
                    "resets_at": 1704067242,
                    "plan_type": "pro"
                }
            })),
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse(vec![
                    responses::ev_response_created("response-b"),
                    responses::ev_assistant_message("message-b", "done"),
                    responses::ev_completed("response-b"),
                ])),
        ],
    )
    .await;
    let test = build_two_account_test(&server, selector.clone(), RuntimePreparation::Ready).await?;
    let initial_credential_revision = test
        .codex
        .execution_account()
        .auth_manager
        .credential_revision();

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "fail over once".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut turn_started = 0;
    let mut turn_completed = 0;
    while turn_completed == 0 {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::TurnStarted(_) => turn_started += 1,
            EventMsg::TurnComplete(_) => turn_completed += 1,
            _ => {}
        }
    }

    let requests = response_mock.requests();
    let evidence = selector
        .evidence
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.header("authorization"))
            .collect::<Vec<_>>(),
        vec![
            Some("Bearer account-a-test-key".to_string()),
            Some("Bearer account-b-test-key".to_string()),
        ]
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| {
                request
                    .message_input_texts("user")
                    .into_iter()
                    .filter(|text| text == "fail over once")
                    .count()
            })
            .collect::<Vec<_>>(),
        vec![1, 1]
    );
    assert_eq!(
        requests[0].body_json()["input"],
        requests[1].body_json()["input"]
    );
    assert_eq!(
        (turn_started, turn_completed, evidence.failover.clone()),
        (
            1,
            1,
            vec![TurnExecutionAccountFailoverSelection {
                selection: TurnExecutionAccountSelection {
                    thread_id: test.session_configured.thread_id,
                    current_binding: ExecutionAccountBinding {
                        slot_id: SLOT_A.to_string(),
                        generation: 1,
                    },
                    account_rotation_policy: ThreadAccountRotationPolicy::virtual_fixed(
                        &ExecutionAccountBinding {
                            slot_id: SLOT_A.to_string(),
                            generation: 1,
                        },
                    ),
                    credential_revision: initial_credential_revision,
                },
                rejected_slot_id: SLOT_A.to_string(),
                rejection_kind: AccountRejectionKind::UsageLimit,
                excluded_account_slot_ids: BTreeSet::from([SLOT_A.to_string()]),
            }],
        )
    );
    assert_eq!(
        evidence.commits,
        vec![TurnExecutionAccountSuccessCommit {
            thread_id: test.session_configured.thread_id,
            expected_binding: ExecutionAccountBinding {
                slot_id: SLOT_A.to_string(),
                generation: 1,
            },
            target_slot_id: SLOT_B.to_string(),
            binding_transition: SuccessfulAccountBindingTransition::AdvanceGeneration,
        }]
    );
    assert_eq!(test.codex.execution_account().binding.slot_id, SLOT_B);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_semantic_response_prevents_account_failover() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let selector = Arc::new(FailOnceSelector {
        evidence: Mutex::new(SelectorEvidence::default()),
    });
    let response_mock = mount_response_sequence(
        &server,
        vec![
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse(vec![
                    responses::ev_message_item_added("partial-message", "partial output"),
                    json!({
                        "type": "response.failed",
                        "response": {
                            "id": "response-a",
                            "error": {
                                "code": "insufficient_quota",
                                "message": "quota exceeded"
                            }
                        }
                    }),
                ])),
        ],
    )
    .await;
    let test = build_two_account_test(&server, selector.clone(), RuntimePreparation::Ready).await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "do not fail over after output".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let evidence = selector
        .evidence
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(response_mock.requests().len(), 1);
    assert_eq!(evidence.failover, Vec::new());
    assert_eq!(evidence.commits, Vec::new());
    assert_eq!(test.codex.execution_account().binding.slot_id, SLOT_A);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_during_failover_runtime_preparation_aborts_without_retry_or_commit() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let selector = Arc::new(FailOnceSelector {
        evidence: Mutex::new(SelectorEvidence::default()),
    });
    let preparation_started = Arc::new(Notify::new());
    let response_mock = mount_response_sequence(
        &server,
        vec![ResponseTemplate::new(429).set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "limit reached",
                "resets_at": 1704067242,
                "plan_type": "pro"
            }
        }))],
    )
    .await;
    let test = build_two_account_test(
        &server,
        selector.clone(),
        RuntimePreparation::Block(Arc::clone(&preparation_started)),
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "cancel failover preparation".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    preparation_started.notified().await;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;

    let evidence = selector
        .evidence
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(response_mock.requests().len(), 1);
    assert_eq!(evidence.failover.len(), 1);
    assert_eq!(evidence.commits, Vec::new());
    assert_eq!(test.codex.execution_account().binding.slot_id, SLOT_A);

    Ok(())
}
