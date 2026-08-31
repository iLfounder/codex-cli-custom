use super::*;

#[tokio::test]
async fn strict_relinquish_releases_exact_generation_for_resume() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db");
    let thread_id = ThreadId::default();
    let first = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    let mut params = create_thread_params(thread_id);
    params.history_mode = ThreadHistoryMode::Paginated;
    first.create_thread(params).await.expect("create thread");
    first
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist session metadata");
    first
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![user_message_item("strict-success")],
        })
        .await
        .expect("append durable item");
    let rollout_path = first
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    let generation = first
        .runtime_snapshot(thread_id)
        .await
        .expect("runtime snapshot")
        .writer_generation
        .expect("writer generation");

    first
        .relinquish_thread(thread_id, generation)
        .await
        .expect("strict release");
    let second = LocalThreadStore::new(config, Some(runtime));
    second
        .resume_thread(ResumeThreadParams {
            thread_id,
            rollout_path: Some(rollout_path),
            history: None,
            include_archived: true,
            metadata: thread_metadata(),
        })
        .await
        .expect("resume released thread");
    let second_generation = second
        .runtime_snapshot(thread_id)
        .await
        .expect("second snapshot")
        .writer_generation
        .expect("second generation");
    assert!(second_generation > generation);
}

#[tokio::test]
async fn strict_relinquish_failure_retains_writer() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db");
    let thread_id = ThreadId::default();
    let first = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    let mut params = create_thread_params(thread_id);
    params.history_mode = ThreadHistoryMode::Paginated;
    first.create_thread(params).await.expect("create thread");
    first
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist session metadata");
    first
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![user_message_item("strict-failure")],
        })
        .await
        .expect("append durable item");
    first
        .flush_thread(thread_id)
        .await
        .expect("initialize projection");
    let generation = first
        .runtime_snapshot(thread_id)
        .await
        .expect("runtime snapshot")
        .writer_generation
        .expect("writer generation");
    first
        .thread_history_db
        .get()
        .expect("projection pool")
        .close()
        .await;

    first
        .relinquish_thread(thread_id, generation)
        .await
        .expect_err("closed projection must fail release");
    let second = LocalThreadStore::new(config, Some(runtime));
    let competing = second
        .resume_thread(ResumeThreadParams {
            thread_id,
            rollout_path: Some(
                first
                    .live_rollout_path(thread_id)
                    .await
                    .expect("live rollout path"),
            ),
            history: None,
            include_archived: true,
            metadata: thread_metadata(),
        })
        .await;
    assert!(matches!(competing, Err(ThreadStoreError::Conflict { .. })));
}
