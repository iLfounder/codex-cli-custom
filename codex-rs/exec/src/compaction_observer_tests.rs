use super::*;
use codex_app_server_protocol::TokenUsageBreakdown as ApiTokenUsageBreakdown;
use pretty_assertions::assert_eq;

fn usage(seed: i64) -> ThreadTokenUsage {
    ThreadTokenUsage {
        last: ApiTokenUsageBreakdown {
            total_tokens: seed,
            input_tokens: seed + 1,
            cached_input_tokens: seed + 2,
            cache_write_input_tokens: seed + 3,
            output_tokens: seed + 4,
            reasoning_output_tokens: seed + 5,
        },
        total: ApiTokenUsageBreakdown {
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

fn mapped_usage(seed: i64) -> ContextCompactionUsage {
    map_token_usage(usage(seed))
}

fn compaction_item(
    id: &str,
    status: ContextCompactionStatus,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
    before: Option<ContextCompactionUsage>,
    latest_reported: Option<ContextCompactionUsage>,
) -> ThreadItem {
    ThreadItem {
        id: id.to_string(),
        details: ThreadItemDetails::ContextCompaction(ContextCompactionItem {
            status,
            started_at_ms,
            completed_at_ms,
            duration_ms: started_at_ms.zip(completed_at_ms).and_then(
                |(started_at_ms, completed_at_ms)| completed_at_ms.checked_sub(started_at_ms),
            ),
            before,
            latest_reported,
        }),
    }
}

#[test]
fn source_observed_sequences_emit_truthful_full_replacement_items() {
    let mut observer = CompactionObserver::default();

    assert_eq!(
        observer.observe_token_usage("thread-1".to_string(), "turn-1".to_string(), usage(100),),
        None
    );
    assert_eq!(
        observer.start(
            "raw-1".to_string(),
            "item_0".to_string(),
            "thread-1".to_string(),
            "turn-1".to_string(),
            /*started_at_ms*/ 1_000,
        ),
        vec![ThreadEvent::ItemStarted(ItemStartedEvent {
            item: compaction_item(
                "item_0",
                ContextCompactionStatus::Compacting,
                /*started_at_ms*/ Some(1_000),
                /*completed_at_ms*/ None,
                /*before*/ Some(mapped_usage(100)),
                /*latest_reported*/ None,
            ),
        })]
    );

    assert_eq!(
        observer.observe_token_usage("thread-1".to_string(), "turn-1".to_string(), usage(200),),
        Some(ThreadEvent::ItemUpdated(ItemUpdatedEvent {
            item: compaction_item(
                "item_0",
                ContextCompactionStatus::Compacting,
                /*started_at_ms*/ Some(1_000),
                /*completed_at_ms*/ None,
                /*before*/ Some(mapped_usage(100)),
                /*latest_reported*/ Some(mapped_usage(200)),
            ),
        }))
    );
    assert_eq!(
        observer.observe_token_usage("thread-1".to_string(), "turn-1".to_string(), usage(200),),
        None,
        "identical consecutive native snapshots are not re-emitted"
    );
    assert_eq!(
        observer.observe_token_usage("thread-1".to_string(), "turn-1".to_string(), usage(128_000),),
        Some(ThreadEvent::ItemUpdated(ItemUpdatedEvent {
            item: compaction_item(
                "item_0",
                ContextCompactionStatus::Compacting,
                /*started_at_ms*/ Some(1_000),
                /*completed_at_ms*/ None,
                /*before*/ Some(mapped_usage(100)),
                /*latest_reported*/ Some(mapped_usage(128_000)),
            ),
        })),
        "a context-window-shaped replacement remains provenance-named latest_reported"
    );
    assert_eq!(
        observer.complete(
            "raw-1",
            /*completed_at_ms*/ 7_500,
            || "unused".to_string(),
        ),
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: compaction_item(
                "item_0",
                ContextCompactionStatus::Completed,
                /*started_at_ms*/ Some(1_000),
                /*completed_at_ms*/ Some(7_500),
                /*before*/ Some(mapped_usage(100)),
                /*latest_reported*/ Some(mapped_usage(128_000)),
            ),
        })
    );
    assert_eq!(observer.turn_completed("thread-1", "turn-1"), None);
    assert_eq!(observer.shutdown(), None);
}

#[test]
fn unfinished_intervals_are_unknown_without_turn_failure_attribution() {
    let mut observer = CompactionObserver::default();
    let _ = observer.start(
        "raw".to_string(),
        "item_0".to_string(),
        "thread".to_string(),
        "turn".to_string(),
        /*started_at_ms*/ 10,
    );

    assert_eq!(
        observer.turn_completed("thread", "turn"),
        Some(ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: compaction_item(
                "item_0",
                ContextCompactionStatus::OutcomeUnknown,
                /*started_at_ms*/ Some(10),
                /*completed_at_ms*/ None,
                /*before*/ None,
                /*latest_reported*/ None,
            ),
        }))
    );
    assert_eq!(observer.shutdown(), None);
}

#[test]
fn new_start_closes_prior_interval_before_opening_a_new_one() {
    let mut observer = CompactionObserver::default();
    let _ = observer.start(
        "raw-1".to_string(),
        "item_0".to_string(),
        "thread".to_string(),
        "turn".to_string(),
        /*started_at_ms*/ 10,
    );
    let _ = observer.observe_token_usage("thread".to_string(), "turn".to_string(), usage(20));

    assert_eq!(
        observer.start(
            "raw-2".to_string(),
            "item_1".to_string(),
            "thread".to_string(),
            "turn".to_string(),
            /*started_at_ms*/ 30,
        ),
        vec![
            ThreadEvent::ItemCompleted(ItemCompletedEvent {
                item: compaction_item(
                    "item_0",
                    ContextCompactionStatus::OutcomeUnknown,
                    /*started_at_ms*/ Some(10),
                    /*completed_at_ms*/ None,
                    /*before*/ None,
                    /*latest_reported*/ Some(mapped_usage(20)),
                ),
            }),
            ThreadEvent::ItemStarted(ItemStartedEvent {
                item: compaction_item(
                    "item_1",
                    ContextCompactionStatus::Compacting,
                    /*started_at_ms*/ Some(30),
                    /*completed_at_ms*/ None,
                    /*before*/ Some(mapped_usage(20)),
                    /*latest_reported*/ None,
                ),
            }),
        ]
    );
}

#[test]
fn completion_without_start_has_null_provenance_and_does_not_close_an_active_peer() {
    let mut observer = CompactionObserver::default();
    let _ = observer.observe_token_usage("thread".to_string(), "turn".to_string(), usage(10));
    let _ = observer.start(
        "active".to_string(),
        "item_0".to_string(),
        "thread".to_string(),
        "turn".to_string(),
        /*started_at_ms*/ 20,
    );

    assert_eq!(
        observer.complete(
            "missing",
            /*completed_at_ms*/ 30,
            || "item_1".to_string(),
        ),
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: compaction_item(
                "item_1",
                ContextCompactionStatus::Completed,
                /*started_at_ms*/ None,
                /*completed_at_ms*/ Some(30),
                /*before*/ None,
                /*latest_reported*/ None,
            ),
        })
    );
    assert!(observer.turn_completed("other-thread", "turn").is_none());
    assert_eq!(
        observer.shutdown(),
        Some(ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: compaction_item(
                "item_0",
                ContextCompactionStatus::OutcomeUnknown,
                /*started_at_ms*/ Some(20),
                /*completed_at_ms*/ None,
                /*before*/ Some(mapped_usage(10)),
                /*latest_reported*/ None,
            ),
        }))
    );
    assert_eq!(observer.shutdown(), None);
}

#[test]
fn duplicate_start_and_other_turn_usage_do_not_change_the_active_interval() {
    let mut observer = CompactionObserver::default();
    let started = observer.start(
        "raw".to_string(),
        "item_0".to_string(),
        "thread".to_string(),
        "turn".to_string(),
        /*started_at_ms*/ 10,
    );
    assert_eq!(started.len(), 1);
    assert_eq!(
        observer.start(
            "raw".to_string(),
            "unused".to_string(),
            "thread".to_string(),
            "turn".to_string(),
            /*started_at_ms*/ 99,
        ),
        Vec::new()
    );
    assert_eq!(
        observer.observe_token_usage("thread".to_string(), "other-turn".to_string(), usage(30),),
        None
    );
    assert_eq!(
        observer.complete("raw", i64::MIN, || "unused".to_string()),
        ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: compaction_item(
                "item_0",
                ContextCompactionStatus::Completed,
                /*started_at_ms*/ Some(10),
                /*completed_at_ms*/ Some(i64::MIN),
                /*before*/ None,
                /*latest_reported*/ None,
            ),
        }),
        "overflowing timestamp subtraction keeps duration null"
    );
}
