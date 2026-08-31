use super::*;
use codex_app_server_protocol::AuthRecoveryNotification;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::trace::TraceId;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::tempdir;
use tracing_opentelemetry::OpenTelemetrySpanExt;

struct ScriptedExecRequestClient {
    responses: Mutex<VecDeque<Result<Value, TypedRequestError>>>,
    requests: Mutex<Vec<ClientRequest>>,
}

impl ScriptedExecRequestClient {
    fn new(responses: impl IntoIterator<Item = Value>) -> Self {
        Self::with_results(responses.into_iter().map(Ok))
    }

    fn with_results(responses: impl IntoIterator<Item = Result<Value, TypedRequestError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ClientRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl ExecRequestClient for ScriptedExecRequestClient {
    fn request_typed<T>(
        &self,
        request: ClientRequest,
    ) -> impl Future<Output = Result<T, TypedRequestError>> + Send
    where
        T: serde::de::DeserializeOwned + Send,
    {
        let method = request.method_name().to_string();
        self.requests.lock().expect("request lock").push(request);
        let response = self
            .responses
            .lock()
            .expect("response lock")
            .pop_front()
            .unwrap_or_else(|| panic!("missing scripted response for {method}"));
        async move {
            serde_json::from_value(response?)
                .map_err(|source| TypedRequestError::Deserialize { method, source })
        }
    }
}

fn account_slot_page(
    account_slot_id: &str,
    account_number: u32,
    registry_revision: u64,
    next_cursor: Option<&str>,
) -> Value {
    serde_json::json!({
        "data": [{
            "accountSlotId": account_slot_id,
            "accountNumber": account_number,
            "label": account_slot_id,
            "isDefault": account_number == 1,
            "status": "ready",
            "health": "healthy",
            "quota": null,
            "authMode": null,
            "attemptGeneration": 1,
            "registryRevision": registry_revision,
            "activeLoginOperationId": null,
            "errorCode": null,
            "actions": [],
            "updatedAt": 0
        }],
        "nextCursor": next_cursor,
        "registryRevision": registry_revision,
        "catalogKind": "global",
        "multiAccount": {"available": true, "denyReason": null}
    })
}

fn rotation_response(revision: u64) -> Value {
    serde_json::json!({
        "rotation": {
            "mode": "fixed",
            "fixedAccountSlotId": "C2",
            "automaticAccountSlotIds": [],
            "revision": revision,
            "lastCommittedAccountSlotId": null
        }
    })
}

fn test_tracing_subscriber() -> impl tracing::Subscriber + Send + Sync {
    let provider = SdkTracerProvider::builder().build();
    let tracer = provider.tracer("codex-exec-tests");
    tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer))
}

#[derive(Clone)]
struct TestLogWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

struct TestLogSink {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TestLogWriter {
    type Writer = TestLogSink;

    fn make_writer(&'a self) -> Self::Writer {
        TestLogSink {
            buffer: Arc::clone(&self.buffer),
        }
    }
}

impl Write for TestLogSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().expect("log buffer lock").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn exec_default_stderr_filter_suppresses_otel_self_diagnostics() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = TestLogWriter {
        buffer: Arc::clone(&buffer),
    };
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(EnvFilter::try_new(EXEC_DEFAULT_LOG_FILTER).expect("default filter")),
    );

    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(target: "opentelemetry_sdk", "telemetry export failed");
        tracing::error!(target: "opentelemetry_otlp", "telemetry request failed");
        tracing::error!(target: "codex_exec_test", "real exec error");
    });

    let logs = String::from_utf8(buffer.lock().expect("log buffer lock").clone()).expect("utf8");
    assert!(!logs.contains("telemetry export failed"));
    assert!(!logs.contains("telemetry request failed"));
    assert!(logs.contains("real exec error"));
}

#[test]
fn exec_root_span_can_be_parented_from_trace_context() {
    let subscriber = test_tracing_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let parent = codex_protocol::protocol::W3cTraceContext {
        traceparent: Some("00-00000000000000000000000000000077-0000000000000088-01".into()),
        tracestate: Some("vendor=value".into()),
    };
    let exec_span = exec_root_span();
    assert!(set_parent_from_w3c_trace_context(&exec_span, &parent));

    let trace_id = exec_span.context().span().span_context().trace_id();
    assert_eq!(
        trace_id,
        TraceId::from_hex("00000000000000000000000000000077").expect("trace id")
    );
}

#[test]
fn builds_uncommitted_review_request() {
    let args = ReviewArgs {
        uncommitted: true,
        base: None,
        commit: None,
        commit_title: None,
        prompt: None,
    };
    let request = build_review_request(&args).expect("builds uncommitted review request");

    let expected = ReviewRequest {
        target: ReviewTarget::UncommittedChanges,
        user_facing_hint: None,
    };

    assert_eq!(request, expected);
}

#[test]
fn builds_commit_review_request_with_title() {
    let args = ReviewArgs {
        uncommitted: false,
        base: None,
        commit: Some("123456789".to_string()),
        commit_title: Some("Add review command".to_string()),
        prompt: None,
    };
    let request = build_review_request(&args).expect("builds commit review request");

    let expected = ReviewRequest {
        target: ReviewTarget::Commit {
            sha: "123456789".to_string(),
            title: Some("Add review command".to_string()),
        },
        user_facing_hint: None,
    };

    assert_eq!(request, expected);
}

#[test]
fn builds_custom_review_request_trims_prompt() {
    let args = ReviewArgs {
        uncommitted: false,
        base: None,
        commit: None,
        commit_title: None,
        prompt: Some("  custom review instructions  ".to_string()),
    };
    let request = build_review_request(&args).expect("builds custom review request");

    let expected = ReviewRequest {
        target: ReviewTarget::Custom {
            instructions: "custom review instructions".to_string(),
        },
        user_facing_hint: None,
    };

    assert_eq!(request, expected);
}

#[test]
fn decode_prompt_bytes_strips_utf8_bom() {
    let input = [0xEF, 0xBB, 0xBF, b'h', b'i', b'\n'];

    let out = decode_prompt_bytes(&input).expect("decode utf-8 with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_decodes_utf16le_bom() {
    // UTF-16LE BOM + "hi\n"
    let input = [0xFF, 0xFE, b'h', 0x00, b'i', 0x00, b'\n', 0x00];

    let out = decode_prompt_bytes(&input).expect("decode utf-16le with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_decodes_utf16be_bom() {
    // UTF-16BE BOM + "hi\n"
    let input = [0xFE, 0xFF, 0x00, b'h', 0x00, b'i', 0x00, b'\n'];

    let out = decode_prompt_bytes(&input).expect("decode utf-16be with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_rejects_utf32le_bom() {
    // UTF-32LE BOM + "hi\n"
    let input = [
        0xFF, 0xFE, 0x00, 0x00, b'h', 0x00, 0x00, 0x00, b'i', 0x00, 0x00, 0x00, b'\n', 0x00, 0x00,
        0x00,
    ];

    let err = decode_prompt_bytes(&input).expect_err("utf-32le should be rejected");

    assert_eq!(
        err,
        PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32LE"
        }
    );
}

#[test]
fn decode_prompt_bytes_rejects_utf32be_bom() {
    // UTF-32BE BOM + "hi\n"
    let input = [
        0x00, 0x00, 0xFE, 0xFF, 0x00, 0x00, 0x00, b'h', 0x00, 0x00, 0x00, b'i', 0x00, 0x00, 0x00,
        b'\n',
    ];

    let err = decode_prompt_bytes(&input).expect_err("utf-32be should be rejected");

    assert_eq!(
        err,
        PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32BE"
        }
    );
}

#[test]
fn decode_prompt_bytes_rejects_invalid_utf8() {
    // Invalid UTF-8 sequence: 0xC3 0x28
    let input = [0xC3, 0x28];

    let err = decode_prompt_bytes(&input).expect_err("invalid utf-8 should fail");

    assert_eq!(err, PromptDecodeError::InvalidUtf8 { valid_up_to: 0 });
}

#[test]
fn prompt_with_stdin_context_wraps_stdin_block() {
    let combined = prompt_with_stdin_context("Summarize this concisely", "my output");

    assert_eq!(
        combined,
        "Summarize this concisely\n\n<stdin>\nmy output\n</stdin>"
    );
}

#[test]
fn prompt_with_stdin_context_preserves_trailing_newline() {
    let combined = prompt_with_stdin_context("Summarize this concisely", "my output\n");

    assert_eq!(
        combined,
        "Summarize this concisely\n\n<stdin>\nmy output\n</stdin>"
    );
}

#[test]
fn lagged_event_warning_message_is_explicit() {
    assert_eq!(
        lagged_event_warning_message(/*skipped*/ 7),
        "in-process app-server event stream lagged; dropped 7 events".to_string()
    );
}

#[tokio::test]
async fn exec_account_failover_bootstrap_precedes_first_turn_and_fails_closed() {
    assert_eq!(
        [
            exec_account_failover_mode(cli::AccountFailover::Disabled),
            exec_account_failover_mode(cli::AccountFailover::PreSemantic),
        ],
        [
            AccountFailoverMode::Disabled,
            AccountFailoverMode::PreSemantic,
        ]
    );

    let client = ScriptedExecRequestClient::new([
        account_slot_page(
            "C2",
            /*account_number*/ 2,
            /*registry_revision*/ 41,
            Some("page-2"),
        ),
        account_slot_page(
            "C1", /*account_number*/ 1, /*registry_revision*/ 41,
            /*next_cursor*/ None,
        ),
        rotation_response(/*revision*/ 7),
        rotation_response(/*revision*/ 8),
        serde_json::json!({
            "turn": {
                "id": "turn-1",
                "items": [],
                "itemsView": "full",
                "status": "inProgress",
                "error": null,
                "startedAt": null,
                "completedAt": null,
                "durationMs": null
            }
        }),
    ]);
    let mut request_ids = RequestIdSequencer::new();
    let response = start_turn_with_account_rotation(
        &client,
        &mut request_ids,
        Some(("thread-1", cli::AccountRotation::RoundRobin)),
        TurnStartParams {
            thread_id: "thread-1".to_string(),
            ..TurnStartParams::default()
        },
    )
    .await
    .expect("stable rotation bootstrap should start the turn");

    assert_eq!(response.turn.id, "turn-1");
    let requests = client.requests();
    assert_eq!(
        requests
            .iter()
            .map(ClientRequest::method_name)
            .collect::<Vec<_>>(),
        [
            "accountSlot/list",
            "accountSlot/list",
            "thread/account/rotation/read",
            "thread/account/rotation/update",
            "turn/start",
        ]
    );
    let ClientRequest::AccountSlotList { params, .. } = &requests[0] else {
        panic!("first request should list account slots");
    };
    assert_eq!(
        params,
        &AccountSlotListParams {
            cursor: None,
            limit: Some(100),
        }
    );
    let ClientRequest::AccountSlotList { params, .. } = &requests[1] else {
        panic!("second request should continue account pagination");
    };
    assert_eq!(
        params,
        &AccountSlotListParams {
            cursor: Some("page-2".to_string()),
            limit: Some(100),
        }
    );
    let ClientRequest::ThreadAccountRotationUpdate { params, .. } = &requests[3] else {
        panic!("fourth request should update account rotation");
    };
    assert_eq!(
        params,
        &ThreadAccountRotationUpdateParams {
            thread_id: "thread-1".to_string(),
            expected_rotation_revision: 7,
            mode: ThreadAccountRotationMode::RoundRobin,
            fixed_account_slot_id: Some("C2".to_string()),
            automatic_account_slot_ids: vec!["C1".to_string(), "C2".to_string()],
        }
    );

    let client = ScriptedExecRequestClient::new([
        account_slot_page(
            "C1",
            /*account_number*/ 1,
            /*registry_revision*/ 41,
            Some("page-2"),
        ),
        account_slot_page(
            "C2", /*account_number*/ 2, /*registry_revision*/ 42,
            /*next_cursor*/ None,
        ),
    ]);
    let error = start_turn_with_account_rotation(
        &client,
        &mut RequestIdSequencer::new(),
        Some(("thread-1", cli::AccountRotation::RoundRobin)),
        TurnStartParams {
            thread_id: "thread-1".to_string(),
            ..TurnStartParams::default()
        },
    )
    .await
    .expect_err("registry revision drift should stop before turn/start");

    assert_eq!(
        error.into_pre_ready_message(),
        "account rotation inventory changed during bootstrap".to_string()
    );
    assert_eq!(
        client
            .requests()
            .iter()
            .map(ClientRequest::method_name)
            .collect::<Vec<_>>(),
        ["accountSlot/list", "accountSlot/list"]
    );
}

#[tokio::test]
async fn account_rotation_bootstrap_errors_are_bounded_and_never_start_a_turn() {
    let client = ScriptedExecRequestClient::with_results([Err(TypedRequestError::Server {
        method: "accountSlot/list".to_string(),
        source: JSONRPCErrorError {
            code: -32603,
            message: "raw slot C7 failure".to_string(),
            data: Some(serde_json::json!({"accountSlotId": "C7"})),
        },
    })]);
    let error = start_turn_with_account_rotation(
        &client,
        &mut RequestIdSequencer::new(),
        Some(("thread-1", cli::AccountRotation::QuotaAware)),
        TurnStartParams {
            thread_id: "thread-1".to_string(),
            ..TurnStartParams::default()
        },
    )
    .await
    .expect_err("account inventory failure should stop before turn/start");

    assert_eq!(
        error.post_ready_message(),
        ACCOUNT_ROTATION_BOOTSTRAP_FAILED
    );
    assert_eq!(
        client
            .requests()
            .iter()
            .map(ClientRequest::method_name)
            .collect::<Vec<_>>(),
        ["accountSlot/list"]
    );

    let client = ScriptedExecRequestClient::with_results([
        Ok(account_slot_page(
            "C1", /*account_number*/ 1, /*registry_revision*/ 41,
            /*next_cursor*/ None,
        )),
        Ok(rotation_response(/*revision*/ 7)),
        Err(TypedRequestError::Server {
            method: "thread/account/rotation/update".to_string(),
            source: JSONRPCErrorError {
                code: -32603,
                message: "raw rotation rejection".to_string(),
                data: None,
            },
        }),
    ]);
    let error = start_turn_with_account_rotation(
        &client,
        &mut RequestIdSequencer::new(),
        Some(("thread-1", cli::AccountRotation::QuotaAware)),
        TurnStartParams {
            thread_id: "thread-1".to_string(),
            ..TurnStartParams::default()
        },
    )
    .await
    .expect_err("rotation update failure should stop before turn/start");

    assert_eq!(
        error.post_ready_message(),
        ACCOUNT_ROTATION_BOOTSTRAP_FAILED
    );
    assert_eq!(
        client
            .requests()
            .iter()
            .map(ClientRequest::method_name)
            .collect::<Vec<_>>(),
        [
            "accountSlot/list",
            "thread/account/rotation/read",
            "thread/account/rotation/update",
        ]
    );
}

#[tokio::test]
async fn turn_start_typed_errors_map_to_bounded_post_ready_codes() {
    let errors = [
        (
            TypedRequestError::Server {
                method: "turn/start".to_string(),
                source: JSONRPCErrorError {
                    code: -32603,
                    message: "raw rejected prompt detail".to_string(),
                    data: Some(serde_json::json!({"accountSlotId": "C9"})),
                },
            },
            TURN_START_REJECTED,
        ),
        (
            TypedRequestError::Transport {
                method: "turn/start".to_string(),
                source: io::Error::new(io::ErrorKind::BrokenPipe, "raw transport detail"),
            },
            TURN_START_OUTCOME_UNKNOWN,
        ),
        (
            TypedRequestError::Deserialize {
                method: "turn/start".to_string(),
                source: serde_json::from_str::<Value>("{")
                    .expect_err("invalid JSON should create a deserialize error"),
            },
            TURN_START_OUTCOME_UNKNOWN,
        ),
    ];

    for (error, expected_code) in errors {
        let client = ScriptedExecRequestClient::with_results([Err(error)]);
        let error = start_turn_with_account_rotation(
            &client,
            &mut RequestIdSequencer::new(),
            None,
            TurnStartParams {
                thread_id: "thread-1".to_string(),
                ..TurnStartParams::default()
            },
        )
        .await
        .expect_err("typed turn/start error should be preserved until the ready boundary");

        assert_eq!(error.post_ready_message(), expected_code);
        assert_eq!(
            client
                .requests()
                .iter()
                .map(ClientRequest::method_name)
                .collect::<Vec<_>>(),
            ["turn/start"]
        );
    }
}

#[test]
fn post_ready_failure_wire_shape_contains_only_the_bounded_message() {
    for message in [
        ACCOUNT_ROTATION_BOOTSTRAP_FAILED,
        TURN_START_REJECTED,
        TURN_START_OUTCOME_UNKNOWN,
    ] {
        let value = serde_json::to_value(ThreadEvent::TurnFailed(TurnFailedEvent {
            error: ThreadErrorEvent {
                message: message.to_string(),
            },
        }))
        .expect("turn.failed should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "turn.failed",
                "error": {"message": message}
            })
        );
    }
}

#[test]
fn invocation_ready_event_is_account_abstract_and_has_stable_wire_shape() {
    let event = invocation_ready_event("turn-01", Some(cli::AccountRotation::QuotaAware));
    let value = serde_json::to_value(ThreadEvent::InvocationReady(event)).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "type": "invocation.ready",
            "protocol_version": 1,
            "invocation_id": "turn-01",
            "provenance": "codex_exec",
            "process_scope": "in_process",
            "capabilities": [
                "ananke_account_rotation_v1",
                "ananke_account_failover_v1"
            ],
            "account_failover": "pre_semantic",
            "account_rotation": {
                "supported": ["quota_aware", "round_robin", "exhaust_then_next"],
                "requested": "quota_aware"
            }
        })
    );

    let resume = invocation_ready_event("resume-01", Some(cli::AccountRotation::RoundRobin));
    assert_eq!(
        resume.account_rotation.requested,
        Some(InvocationReadyRotationMode::RoundRobin)
    );

    let normal_resume = invocation_ready_event("resume-02", None);
    assert_eq!(normal_resume.account_rotation.requested, None);
}

#[test]
fn runtime_warnings_are_filtered_to_the_primary_thread() {
    let primary_thread_id = "thread-1";
    let turn_id = "turn-1";
    let outcomes = [
        codex_app_server_protocol::WarningNotification {
            thread_id: None,
            message: "global warning".to_string(),
        },
        codex_app_server_protocol::WarningNotification {
            thread_id: Some(primary_thread_id.to_string()),
            message: "primary warning".to_string(),
        },
        codex_app_server_protocol::WarningNotification {
            thread_id: Some("thread-2".to_string()),
            message: "other warning".to_string(),
        },
    ]
    .map(|warning| {
        should_process_notification(
            &ServerNotification::Warning(warning),
            primary_thread_id,
            turn_id,
        )
    });

    assert_eq!(outcomes, [true, true, false]);

    let recovery = AuthRecoveryNotification {
        thread_id: primary_thread_id.to_string(),
        turn_id: turn_id.to_string(),
        provider: "example".to_string(),
        message: "Refresh authentication".to_string(),
    };
    let outcomes = [
        ServerNotification::AuthRecoveryStarted(recovery.clone()),
        ServerNotification::AuthRecoveryCompleted(recovery.clone()),
        ServerNotification::AuthRecoveryStarted(AuthRecoveryNotification {
            thread_id: "thread-2".to_string(),
            ..recovery.clone()
        }),
        ServerNotification::AuthRecoveryCompleted(AuthRecoveryNotification {
            turn_id: "turn-2".to_string(),
            ..recovery
        }),
    ]
    .map(|notification| should_process_notification(&notification, primary_thread_id, turn_id));

    assert_eq!(outcomes, [true, true, false, false]);
}

#[tokio::test]
async fn resume_lookup_model_providers_filters_only_last_lookup() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build default config");
    config.model_provider_id = "test-provider".to_string();

    let last_args = crate::cli::ResumeArgs {
        session_id: None,
        last: true,
        all: false,
        images: vec![],
        prompt: None,
    };
    let named_args = crate::cli::ResumeArgs {
        session_id: Some("named-session".to_string()),
        last: false,
        all: false,
        images: vec![],
        prompt: None,
    };

    assert_eq!(
        resume_lookup_model_providers(&config, &last_args),
        Some(vec!["test-provider".to_string()])
    );
    assert_eq!(resume_lookup_model_providers(&config, &named_args), None);
}

#[test]
fn turn_items_for_thread_returns_matching_turn_items() {
    let thread = AppServerThread {
        id: "thread-1".to_string(),
        extra: None,
        session_id: "thread-1".to_string(),
        forked_from_id: None,
        parent_thread_id: None,
        preview: String::new(),
        ephemeral: false,
        section: None,
        section_entered_at: None,
        project_id: None,
        history_mode: Default::default(),
        model_provider: "openai".to_string(),
        model: None,
        reasoning_effort: None,
        created_at: 0,
        updated_at: 0,
        recency_at: Some(0),
        status: codex_app_server_protocol::ThreadStatus::Idle,
        path: None,
        cwd: test_path_buf("/tmp/project").abs(),
        cli_version: "0.0.0-test".to_string(),
        source: codex_app_server_protocol::SessionSource::Exec,
        can_accept_direct_input: None,
        thread_source: None,
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        turns: vec![
            codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![AppServerThreadItem::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "hello".to_string(),
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                }],
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
            codex_app_server_protocol::Turn {
                id: "turn-2".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![AppServerThreadItem::Plan {
                    id: "plan-1".to_string(),
                    text: "ship it".to_string(),
                }],
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        ],
    };

    assert_eq!(
        turn_items_for_thread(&thread, "turn-1"),
        Some(vec![AppServerThreadItem::AgentMessage {
            id: "msg-1".to_string(),
            text: "hello".to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        }])
    );
    assert_eq!(turn_items_for_thread(&thread, "missing-turn"), None);
}

#[test]
fn should_backfill_turn_completed_items_backfills_persisted_summaries_only() {
    let notification =
        ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Summary,
                items: Vec::new(),
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        });

    assert!(!should_backfill_turn_completed_items(
        /*thread_ephemeral*/ true,
        &notification
    ));
    assert!(should_backfill_turn_completed_items(
        /*thread_ephemeral*/ false,
        &notification
    ));
}

#[test]
fn canceled_mcp_server_elicitation_response_uses_cancel_action() {
    let value = canceled_mcp_server_elicitation_response()
        .expect("mcp elicitation cancel response should serialize");
    let response: McpServerElicitationRequestResponse =
        serde_json::from_value(value).expect("cancel response should deserialize");

    assert_eq!(
        response,
        McpServerElicitationRequestResponse {
            action: McpServerElicitationAction::Cancel,
            content: None,
            meta: None,
        }
    );
}

#[tokio::test]
async fn thread_start_params_include_review_policy_when_review_policy_is_manual_only() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            approvals_reviewer: Some(ApprovalsReviewer::User),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with manual-only review policy");

    let params = thread_start_params_from_config(&config, &ThreadSource::User);

    assert_eq!(
        params.approvals_reviewer,
        Some(codex_app_server_protocol::ApprovalsReviewer::User)
    );
    assert_eq!(params.sandbox, None);
    assert_eq!(
        params.permissions,
        permissions_selection_from_config(&config)
    );
}

#[tokio::test]
async fn thread_start_params_include_review_policy_when_auto_review_is_enabled() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            approvals_reviewer: Some(ApprovalsReviewer::AutoReview),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with guardian review policy");

    let params = thread_start_params_from_config(&config, &ThreadSource::User);

    assert_eq!(
        params.approvals_reviewer,
        Some(codex_app_server_protocol::ApprovalsReviewer::AutoReview)
    );
}

#[tokio::test]
async fn thread_resume_params_only_include_explicit_review_policy_override() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            approvals_reviewer: Some(ApprovalsReviewer::AutoReview),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with guardian review policy");

    let params_without_override = thread_resume_params_from_config(
        &config,
        "thread-id".to_string(),
        /*approvals_reviewer_override*/ None,
    );
    let params_with_override = thread_resume_params_from_config(
        &config,
        "thread-id".to_string(),
        Some(codex_app_server_protocol::ApprovalsReviewer::AutoReview),
    );

    assert_eq!(params_without_override.approvals_reviewer, None);
    assert_eq!(
        params_with_override.approvals_reviewer,
        Some(codex_app_server_protocol::ApprovalsReviewer::AutoReview)
    );
}

#[tokio::test]
async fn build_exec_config_retries_without_invalid_headless_policy_for_auto_review() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
approval_policy = "on-request"
approvals_reviewer = "auto_review"
"#,
    )
    .expect("write config");
    let requirements_path = codex_home.path().join("requirements.toml");
    std::fs::write(
        &requirements_path,
        r#"
allowed_approval_policies = ["never", "on-request"]
allowed_sandbox_modes = ["read-only", "workspace-write"]
"#,
    )
    .expect("write requirements");
    let mut loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    loader_overrides.system_requirements_path = Some(requirements_path);
    let overrides = ConfigOverrides {
        cwd: Some(cwd.path().to_path_buf()),
        approval_policy: Some(AskForApproval::Never),
        sandbox_mode: Some(SandboxMode::DangerFullAccess),
        ..Default::default()
    };
    let build_config = |overrides| {
        ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .loader_overrides(loader_overrides.clone())
            .harness_overrides(overrides)
            .build()
    };

    let error = build_config(overrides.clone())
        .await
        .expect_err("synthetic headless approval policy should fail");
    assert!(
        error
            .to_string()
            .contains("`approval_policy = \"never\"` cannot be used")
    );

    let config = build_exec_config(
        overrides,
        /*preserve_headless_approval_policy*/ false,
        build_config,
    )
    .await
    .expect("auto-review config should retry without the synthetic approval policy");

    assert_eq!(
        config.permissions.approval_policy.value(),
        AskForApproval::OnRequest
    );
    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::AutoReview);
}

#[tokio::test]
async fn build_exec_config_preserves_headless_error_when_retry_fails() {
    let overrides = ConfigOverrides {
        approval_policy: Some(AskForApproval::Never),
        ..Default::default()
    };

    let error = build_exec_config(
        overrides,
        /*preserve_headless_approval_policy*/ false,
        |overrides| async move {
            let message = if overrides.approval_policy == Some(AskForApproval::Never) {
                "headless error"
            } else {
                "retry error"
            };
            Err(std::io::Error::other(message))
        },
    )
    .await
    .expect_err("failed speculative retry should preserve the original error");

    assert_eq!(error.to_string(), "headless error");
}

#[tokio::test]
async fn thread_start_params_match_history_to_persistence() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");

    let params = thread_start_params_from_config(&config, &ThreadSource::User);

    assert_eq!(
        params.thread_source,
        Some(codex_app_server_protocol::ThreadSource::User)
    );
    assert_eq!(params.history_mode, Some(ThreadHistoryMode::Paginated));

    let thread_source = ThreadSource::Feature("automated_review".to_string());
    let params = thread_start_params_from_config(&config, &thread_source);
    assert_eq!(params.thread_source, Some(thread_source));

    config.ephemeral = true;
    let params = thread_start_params_from_config(&config, &ThreadSource::User);

    assert_eq!(params.ephemeral, Some(true));
    assert_eq!(params.history_mode, None);
}

#[tokio::test]
async fn thread_lifecycle_params_preserve_hook_trust_bypass() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            bypass_hook_trust: Some(true),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with hook trust bypass");
    let expected_config = Some(HashMap::from([(
        "bypass_hook_trust".to_string(),
        serde_json::Value::Bool(true),
    )]));

    let start_params = thread_start_params_from_config(&config, &ThreadSource::User);
    let resume_params = thread_resume_params_from_config(
        &config,
        "thread-id".to_string(),
        /*approvals_reviewer_override*/ None,
    );

    assert_eq!(start_params.config, expected_config);
    assert_eq!(resume_params.config, expected_config);
}

#[test]
fn active_profile_selection_uses_profile_id_only() {
    let selection = permission_profile_id_from_active_profile(ActivePermissionProfile::new(
        BUILT_IN_PERMISSION_PROFILE_WORKSPACE,
    ));

    assert_eq!(selection, BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string());
}

#[tokio::test]
async fn thread_lifecycle_params_include_legacy_sandbox_when_no_active_profile() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with legacy sandbox override");

    let start_params = thread_start_params_from_config(&config, &ThreadSource::User);
    let resume_params = thread_resume_params_from_config(
        &config,
        "thread-id".to_string(),
        /*approvals_reviewer_override*/ None,
    );

    assert_eq!(config.permissions.active_permission_profile(), None);
    assert_eq!(
        start_params.sandbox,
        Some(codex_app_server_protocol::SandboxMode::DangerFullAccess)
    );
    assert_eq!(start_params.permissions, None);
    assert_eq!(
        resume_params.sandbox,
        Some(codex_app_server_protocol::SandboxMode::DangerFullAccess)
    );
    assert_eq!(resume_params.permissions, None);
}

#[tokio::test]
async fn session_configured_from_thread_response_uses_review_policy_from_response() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let response = sample_thread_start_response();

    let event = session_configured_from_thread_start_response(&response, &config)
        .expect("build bootstrap session configured event");

    assert_eq!(
        event.session_id.to_string(),
        "67e55044-10b1-426f-9247-bb680e5fe0c7"
    );
    assert_eq!(
        event.thread_id.to_string(),
        "67e55044-10b1-426f-9247-bb680e5fe0c8"
    );
    assert_eq!(event.approvals_reviewer, ApprovalsReviewer::AutoReview);
}

#[tokio::test]
async fn session_configured_from_thread_response_uses_permission_profile_from_config() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let response = sample_thread_start_response();

    let event = session_configured_from_thread_start_response(&response, &config)
        .expect("build bootstrap session configured event");

    assert_eq!(
        event.permission_profile,
        config.permissions.effective_permission_profile()
    );
}

#[tokio::test]
async fn session_configured_from_thread_response_preserves_thread_source() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let response = sample_thread_start_response();

    let event = session_configured_from_thread_start_response(&response, &config)
        .expect("build bootstrap session configured event");

    assert_eq!(
        event.thread_source,
        Some(codex_protocol::protocol::ThreadSource::User)
    );
}

#[tokio::test]
async fn session_configured_from_thread_response_preserves_parent_thread_id() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let parent_thread_id = ThreadId::new();
    let forked_from_id = ThreadId::new();
    let mut response = sample_thread_start_response();
    response.thread.parent_thread_id = Some(parent_thread_id.to_string());
    response.thread.forked_from_id = Some(forked_from_id.to_string());

    let event = session_configured_from_thread_start_response(&response, &config)
        .expect("build bootstrap session configured event");

    assert_eq!(event.parent_thread_id, Some(parent_thread_id));
    assert_eq!(event.forked_from_id, Some(forked_from_id));
}

fn sample_thread_start_response() -> ThreadStartResponse {
    ThreadStartResponse {
        thread: codex_app_server_protocol::Thread {
            id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
            extra: None,
            session_id: "67e55044-10b1-426f-9247-bb680e5fe0c7".to_string(),
            forked_from_id: None,
            parent_thread_id: None,
            preview: String::new(),
            ephemeral: false,
            section: None,
            section_entered_at: None,
            project_id: None,
            history_mode: Default::default(),
            model_provider: "openai".to_string(),
            model: None,
            reasoning_effort: None,
            created_at: 0,
            updated_at: 0,
            recency_at: Some(0),
            status: codex_app_server_protocol::ThreadStatus::Idle,
            path: Some(PathBuf::from("/tmp/rollout.jsonl")),
            cwd: test_path_buf("/tmp").abs(),
            cli_version: "0.0.0".to_string(),
            source: codex_app_server_protocol::SessionSource::Cli,
            can_accept_direct_input: None,
            thread_source: Some(codex_app_server_protocol::ThreadSource::User),
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: Some("thread".to_string()),
            turns: vec![],
        },
        transition: None,
        model: "gpt-5.4".to_string(),
        model_provider: "openai".to_string(),
        service_tier: None,
        cwd: test_path_buf("/tmp").abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_sources: Vec::new(),
        approval_policy: codex_app_server_protocol::AskForApproval::OnRequest,
        approvals_reviewer: codex_app_server_protocol::ApprovalsReviewer::AutoReview,
        sandbox: codex_app_server_protocol::SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        },
        active_permission_profile: None,
        reasoning_effort: None,
        multi_agent_mode: Default::default(),
    }
}
