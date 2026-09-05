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
async fn projection_failure_releases_writer_with_degraded_evidence() {
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
    let rollout_path = first
        .live_rollout_path(thread_id)
        .await
        .expect("live rollout path");
    first
        .thread_history_db
        .get()
        .expect("projection pool")
        .close()
        .await;

    let failure_guard = first.projection_failures.lock().await;
    let release_store = first.clone();
    let release =
        tokio::spawn(async move { release_store.relinquish_thread(thread_id, generation).await });
    while first.live_recorders.lock().await.contains_key(&thread_id) {
        tokio::task::yield_now().await;
    }
    let coordination = first.live_writer_locks.coordination(thread_id).await;
    assert!(
        coordination.writer.try_lock().is_err(),
        "writer fence must remain held until projection evidence is committed"
    );
    drop(failure_guard);
    release
        .await
        .expect("release task")
        .expect("projection failure must not retain writer");
    let released = first
        .runtime_snapshot(thread_id)
        .await
        .expect("released runtime snapshot");
    assert_eq!(
        (
            released.writer_ownership,
            released.flush_health,
            released.materialize_health,
            released.persistence_deny_reason.as_deref(),
        ),
        (
            RuntimeWriterOwnership::None,
            RuntimePersistenceHealth::Healthy,
            RuntimePersistenceHealth::Degraded,
            Some(super::PROJECTION_FAILED_REASON),
        )
    );
    let repeated = first
        .runtime_snapshot(thread_id)
        .await
        .expect("repeated released runtime snapshot");
    assert_eq!(
        (
            repeated.writer_ownership,
            repeated.flush_health,
            repeated.materialize_health,
            repeated.persistence_deny_reason.as_deref(),
        ),
        (
            RuntimeWriterOwnership::None,
            RuntimePersistenceHealth::Healthy,
            RuntimePersistenceHealth::Degraded,
            Some(super::PROJECTION_FAILED_REASON),
        )
    );
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
        .expect("released writer must be acquirable");
    let next_generation = second
        .runtime_snapshot(thread_id)
        .await
        .expect("next runtime snapshot")
        .writer_generation
        .expect("next writer generation");
    assert!(next_generation > generation);
}

#[tokio::test]
async fn authoritative_flush_failure_retains_writer() {
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
    let generation = first
        .runtime_snapshot(thread_id)
        .await
        .expect("runtime snapshot")
        .writer_generation
        .expect("writer generation");
    let (recorder, _, _) = live_writer::live_writer_parts(&first, thread_id)
        .await
        .expect("live recorder");
    recorder.shutdown().await.expect("stop writer task");

    first
        .relinquish_thread(thread_id, generation)
        .await
        .expect_err("authoritative flush failure must fail release");
    let retained = first
        .runtime_snapshot(thread_id)
        .await
        .expect_err("closed retained recorder cannot report progress");
    assert!(matches!(retained, ThreadStoreError::Internal { .. }));
    let second = LocalThreadStore::new(config, Some(runtime));
    let competing = second
        .resume_thread(ResumeThreadParams {
            thread_id,
            rollout_path: Some(
                first
                    .live_rollout_path(thread_id)
                    .await
                    .expect("retained rollout path"),
            ),
            history: None,
            include_archived: true,
            metadata: thread_metadata(),
        })
        .await;
    assert!(matches!(competing, Err(ThreadStoreError::Conflict { .. })));
}
