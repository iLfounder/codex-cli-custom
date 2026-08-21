use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::LocalThreadStore;
use super::STATE_DB_POSITION_UNAVAILABLE_REASON;
use super::test_support::test_config;
use crate::AppendThreadItemsParams;
use crate::CreateThreadParams;
use crate::RuntimePersistenceHealth;
use crate::RuntimeWriterOwnership;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;

#[tokio::test]
async fn live_paginated_snapshot_reports_exact_jsonl_and_state_db_deny() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    store
        .create_thread(create_thread_params(thread_id))
        .await
        .expect("create live thread");
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![user_message_item("persisted")],
        })
        .await
        .expect("append item");
    store
        .flush_thread(thread_id)
        .await
        .expect("flush live thread");

    let snapshot = store
        .runtime_snapshot(thread_id)
        .await
        .expect("runtime snapshot");
    assert_eq!(snapshot.writer_ownership, RuntimeWriterOwnership::OwnedHere);
    assert_eq!(
        snapshot.writer_deny_reason.as_deref(),
        Some(super::WRITER_CONTROL_UNAVAILABLE_REASON)
    );
    assert!(
        snapshot.jsonl.is_some_and(|position| position.offset > 0),
        "live recorder must expose its durable byte position"
    );
    assert_eq!(snapshot.jsonl.map(|position| position.ordinal), Some(2));
    assert_eq!(snapshot.sqlite, None);
    assert_eq!(snapshot.lag, None);
    assert_eq!(snapshot.flush_health, RuntimePersistenceHealth::Healthy);
    assert_eq!(
        snapshot.persistence_deny_reason.as_deref(),
        Some(STATE_DB_POSITION_UNAVAILABLE_REASON)
    );

    store.shutdown_thread(thread_id).await.expect("shutdown");
}

fn create_thread_params(thread_id: ThreadId) -> CreateThreadParams {
    CreateThreadParams {
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "test_originator".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Paginated,
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: uuid::Uuid::now_v7().to_string(),
        metadata: ThreadPersistenceMetadata {
            cwd: Some(std::env::current_dir().expect("cwd")),
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn user_message_item(message: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: message.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}
