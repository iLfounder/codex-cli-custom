use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

fn token_usage(seed: i64) -> ThreadTokenUsage {
    ThreadTokenUsage {
        last: codex_app_server_protocol::TokenUsageBreakdown {
            total_tokens: seed,
            input_tokens: seed + 1,
            cached_input_tokens: seed + 2,
            cache_write_input_tokens: seed + 3,
            output_tokens: seed + 4,
            reasoning_output_tokens: seed + 5,
        },
        total: codex_app_server_protocol::TokenUsageBreakdown {
            total_tokens: seed + 10,
            input_tokens: seed + 11,
            cached_input_tokens: seed + 12,
            cache_write_input_tokens: seed + 13,
            output_tokens: seed + 14,
            reasoning_output_tokens: seed + 15,
        },
        model_context_window: Some(seed + 100),
    }
}

#[test]
fn failed_turn_does_not_overwrite_output_last_message_file() {
    let tempdir = tempdir().expect("create tempdir");
    let output_path = tempdir.path().join("last-message.txt");
    std::fs::write(&output_path, "keep existing contents").expect("seed output file");

    let mut processor = EventProcessorWithJsonOutput::new(Some(output_path.clone()));

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: "msg-1".to_string(),
                text: "partial answer".to_string(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        },
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(processor.final_message(), Some("partial answer"));

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Failed,
                error: Some(codex_app_server_protocol::TurnError {
                    misalignment: None,
                    message: "turn failed".to_string(),
                    additional_details: None,
                    codex_error_info: None,
                }),
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(status, CodexStatus::InitiateShutdown);
    assert_eq!(processor.final_message(), None);

    EventProcessor::print_final_output(&mut processor);

    assert_eq!(
        std::fs::read_to_string(&output_path).expect("read output file"),
        "keep existing contents"
    );
}

#[test]
fn response_stream_disconnect_turn_failure_uses_safe_code() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Failed,
                error: Some(codex_app_server_protocol::TurnError {
                    misalignment: None,
                    message: "raw stream failure".to_string(),
                    additional_details: Some("private transport detail".to_string()),
                    codex_error_info: Some(CodexErrorInfo::ResponseStreamDisconnected {
                        http_status_code: None,
                    }),
                }),
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        collected,
        CollectedThreadEvents {
            events: vec![ThreadEvent::TurnFailed(TurnFailedEvent {
                error: ThreadErrorEvent {
                    message: "codex_exec_response_stream_disconnected".to_string(),
                },
            })],
            status: CodexStatus::InitiateShutdown,
        }
    );
}

#[test]
fn runtime_warning_emits_a_non_fatal_error_item() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::Warning(
        codex_app_server_protocol::WarningNotification {
            thread_id: Some("thread-1".to_string()),
            message: "invalid global instructions".to_string(),
        },
    ));

    assert_eq!(
        collected,
        CollectedThreadEvents {
            events: vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
                item: ExecThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::Error(ErrorItem {
                        message: "invalid global instructions".to_string(),
                    }),
                },
            })],
            status: CodexStatus::Running,
        }
    );
}

#[test]
fn mcp_tool_call_result_preserves_meta_in_jsonl_event() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::McpToolCall {
                id: "mcp-1".to_string(),
                server: "search service".to_string(),
                tool: "web_run".to_string(),
                status: McpToolCallStatus::Completed,
                arguments: json!({"search_query": [{"q": "OpenAI Codex CLI documentation"}]}),
                app_context: None,
                mcp_app_resource_uri: None,
                plugin_id: None,
                read_only_hint: None,
                result: Some(Box::new(codex_app_server_protocol::McpToolCallResult {
                    content: vec![json!({"type": "text", "text": "search result"})],
                    structured_content: None,
                    meta: Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]})),
                })),
                error: None,
                duration_ms: Some(42),
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        },
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(collected.events.len(), 1);

    let ThreadEvent::ItemCompleted(ItemCompletedEvent { item }) = &collected.events[0] else {
        panic!("expected item.completed event");
    };
    let ThreadItemDetails::McpToolCall(item) = &item.details else {
        panic!("expected MCP tool call item");
    };
    let result = item.result.as_ref().expect("expected MCP tool result");
    assert_eq!(
        result.meta,
        Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]}))
    );

    let serialized = serde_json::to_value(&collected.events[0]).expect("serialize event");
    assert_eq!(
        serialized["item"]["result"]["_meta"],
        json!({"raw_messages": [{"ref_id": "turn0search0"}]})
    );
    assert!(serialized["item"]["result"].get("meta").is_none());
}

#[test]
fn compaction_processor_emits_full_lifecycle_and_retry_error_without_relabeling() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);
    assert_eq!(
        processor
            .collect_thread_events(ServerNotification::ThreadTokenUsageUpdated(
                codex_app_server_protocol::ThreadTokenUsageUpdatedNotification {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    token_usage: token_usage(10),
                },
            ))
            .events,
        Vec::new()
    );

    let started = processor.collect_thread_events(ServerNotification::ItemStarted(
        codex_app_server_protocol::ItemStartedNotification {
            item: ThreadItem::ContextCompaction {
                id: "compact-1".to_string(),
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            started_at_ms: 1_000,
        },
    ));
    assert_eq!(
        serde_json::to_value(&started.events).expect("serialize started event"),
        json!([{
            "type": "item.started",
            "item": {
                "id": "item_0",
                "type": "context_compaction",
                "status": "compacting",
                "started_at_ms": 1_000,
                "completed_at_ms": null,
                "duration_ms": null,
                "before": {
                    "reported_last_usage": {
                        "total_tokens": 10,
                        "input_tokens": 11,
                        "cached_input_tokens": 12,
                        "cache_write_input_tokens": 13,
                        "output_tokens": 14,
                        "reasoning_output_tokens": 15,
                    },
                    "reported_total_usage": {
                        "total_tokens": 20,
                        "input_tokens": 21,
                        "cached_input_tokens": 22,
                        "cache_write_input_tokens": 23,
                        "output_tokens": 24,
                        "reasoning_output_tokens": 25,
                    },
                    "model_context_window": 110,
                },
                "latest_reported": null,
            }
        }])
    );

    let updated = processor.collect_thread_events(ServerNotification::ThreadTokenUsageUpdated(
        codex_app_server_protocol::ThreadTokenUsageUpdatedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            token_usage: token_usage(30),
        },
    ));
    assert_eq!(updated.events.len(), 1);
    assert!(matches!(updated.events[0], ThreadEvent::ItemUpdated(_)));

    let retry = processor.collect_thread_events(ServerNotification::Error(
        codex_app_server_protocol::ErrorNotification {
            error: codex_app_server_protocol::TurnError {
                misalignment: None,
                message: "retrying summarizer".to_string(),
                codex_error_info: None,
                additional_details: None,
            },
            will_retry: true,
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
        },
    ));
    assert_eq!(
        retry.events,
        vec![ThreadEvent::Error(ThreadErrorEvent {
            message: "retrying summarizer".to_string(),
        })]
    );

    let completed = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::ContextCompaction {
                id: "compact-1".to_string(),
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 7_500,
        },
    ));
    let serialized = serde_json::to_value(&completed.events[0]).expect("serialize completed event");
    assert_eq!(serialized["type"], "item.completed");
    assert_eq!(serialized["item"]["status"], "completed");
    assert_eq!(serialized["item"]["duration_ms"], 6_500);
    assert_eq!(
        serialized["item"]["latest_reported"]["reported_last_usage"]["total_tokens"],
        30
    );
    assert_eq!(processor.collect_final_events(), Vec::new());
}

#[test]
fn completion_without_start_and_stream_shutdown_preserve_missing_boundaries() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);
    let _ = processor.collect_thread_events(ServerNotification::ThreadTokenUsageUpdated(
        codex_app_server_protocol::ThreadTokenUsageUpdatedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            token_usage: token_usage(10),
        },
    ));

    let missing_start = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::ContextCompaction {
                id: "missing-start".to_string(),
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 2_000,
        },
    ));
    assert_eq!(
        serde_json::to_value(&missing_start.events[0]).expect("serialize missing-start event"),
        json!({
            "type": "item.completed",
            "item": {
                "id": "item_0",
                "type": "context_compaction",
                "status": "completed",
                "started_at_ms": null,
                "completed_at_ms": 2_000,
                "duration_ms": null,
                "before": null,
                "latest_reported": null,
            }
        })
    );

    let _ = processor.collect_thread_events(ServerNotification::ItemStarted(
        codex_app_server_protocol::ItemStartedNotification {
            item: ThreadItem::ContextCompaction {
                id: "unfinished".to_string(),
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            started_at_ms: 3_000,
        },
    ));
    let shutdown = processor.collect_final_events();
    assert_eq!(shutdown.len(), 1);
    assert_eq!(
        serde_json::to_value(&shutdown[0]).expect("serialize shutdown terminal"),
        json!({
            "type": "item.completed",
            "item": {
                "id": "item_1",
                "type": "context_compaction",
                "status": "outcome_unknown",
                "started_at_ms": 3_000,
                "completed_at_ms": null,
                "duration_ms": null,
                "before": {
                    "reported_last_usage": {
                        "total_tokens": 10,
                        "input_tokens": 11,
                        "cached_input_tokens": 12,
                        "cache_write_input_tokens": 13,
                        "output_tokens": 14,
                        "reasoning_output_tokens": 15,
                    },
                    "reported_total_usage": {
                        "total_tokens": 20,
                        "input_tokens": 21,
                        "cached_input_tokens": 22,
                        "cache_write_input_tokens": 23,
                        "output_tokens": 24,
                        "reasoning_output_tokens": 25,
                    },
                    "model_context_window": 110,
                },
                "latest_reported": null,
            }
        })
    );
    assert_eq!(processor.collect_final_events(), Vec::new());
}
