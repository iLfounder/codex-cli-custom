use std::fs;

use pretty_assertions::assert_eq;

use super::*;
use crate::PrepareThreadResumeTarget;

#[tokio::test]
async fn prepare_acquires_writer_authority_before_reading_history() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let first = LocalThreadStore::new(config.clone(), /*state_db*/ None);
    let thread_id = ThreadId::default();
    first
        .create_thread(create_thread_params(thread_id))
        .await
        .expect("create thread");
    first
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist thread");
    let rollout_path = first
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    first
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown writer");

    let _writer = first
        .acquire_writer_lock(thread_id)
        .await
        .expect("reserve writer");
    fs::remove_file(&rollout_path).expect("remove rollout in isolated temp dir");
    let competing = LocalThreadStore::new(config, /*state_db*/ None);
    let error = competing
        .prepare_thread_resume(PrepareThreadResumeParams {
            target: PrepareThreadResumeTarget::ThreadId(thread_id),
            include_archived: true,
        })
        .await
        .expect_err("competing resume must stop at writer authority");

    assert!(matches!(error, ThreadStoreError::Conflict { .. }));
}

#[tokio::test]
async fn prepared_snapshot_hands_writer_authority_to_live_recorder() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::default();
    store
        .create_thread(create_thread_params(thread_id))
        .await
        .expect("create thread");
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![user_message_item("before resume")],
        })
        .await
        .expect("append initial item");
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist thread");
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown writer");

    let prepared = store
        .prepare_thread_resume(PrepareThreadResumeParams {
            target: PrepareThreadResumeTarget::ThreadId(thread_id),
            include_archived: true,
        })
        .await
        .expect("prepare resume");
    let (stored_thread, snapshot, authority) = prepared.into_parts();
    assert_eq!(stored_thread.thread_id, thread_id);
    assert!(snapshot.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::UserMessage(event))
                if event.message == "before resume"
        )
    }));

    store
        .activate_prepared_thread_resume(
            authority,
            ResumeThreadParams {
                thread_id,
                rollout_path: stored_thread.rollout_path,
                history: Some(snapshot),
                include_archived: true,
                metadata: thread_metadata(),
            },
        )
        .await
        .expect("activate prepared writer");
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![user_message_item("after resume")],
        })
        .await
        .expect("append through resumed writer");
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist resumed thread");
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown resumed writer");

    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id,
            include_archived: true,
        })
        .await
        .expect("load final history");
    assert!(history.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::UserMessage(event))
                if event.message == "after resume"
        )
    }));
}
