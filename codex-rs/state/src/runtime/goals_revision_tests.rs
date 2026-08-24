use super::*;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

async fn test_runtime() -> Arc<StateRuntime> {
    StateRuntime::init(
        crate::SqliteConfig::new_for_testing(unique_temp_dir().as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state db should initialize")
}

fn test_thread_id() -> ThreadId {
    ThreadId::from_string("00000000-0000-0000-0000-000000000321").expect("valid thread id")
}

#[tokio::test]
async fn exact_goal_mutations_advance_revision_and_preserve_clear_tombstone() {
    let runtime = test_runtime().await;
    let thread_id = test_thread_id();
    let first = runtime
        .thread_goals()
        .create_thread_goal_exact(
            thread_id,
            /*expected_revision*/ 0,
            "first objective",
            crate::ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await
        .expect("create should succeed")
        .expect("revision zero should match empty state");
    assert_eq!(1, first.revision);

    let updated = runtime
        .thread_goals()
        .update_thread_goal_exact(
            thread_id,
            &GoalVersion::from(&first),
            GoalUpdate {
                objective: None,
                status: Some(crate::ThreadGoalStatus::Blocked),
                token_budget: None,
                expected_goal_id: Some(first.goal_id.clone()),
            },
        )
        .await
        .expect("update should succeed")
        .expect("exact version should match");
    assert_eq!(2, updated.revision);

    let cleared = runtime
        .thread_goals()
        .clear_thread_goal_exact(thread_id, &GoalVersion::from(&updated))
        .await
        .expect("clear should succeed")
        .expect("exact version should clear");
    assert_eq!(3, cleared.revision);
    assert_eq!(
        3,
        runtime
            .thread_goals()
            .get_thread_goal_revision(thread_id)
            .await
            .unwrap()
    );

    let second = runtime
        .thread_goals()
        .create_thread_goal_exact(
            thread_id,
            /*expected_revision*/ 3,
            "second objective",
            crate::ThreadGoalStatus::Active,
            /*token_budget*/ Some(100),
        )
        .await
        .expect("recreate should succeed")
        .expect("tombstone revision should match");
    assert_eq!(4, second.revision);
    assert_ne!(first.goal_id, second.goal_id);

    assert_eq!(
        None,
        runtime
            .thread_goals()
            .update_thread_goal_exact(
                thread_id,
                &GoalVersion::from(&first),
                GoalUpdate {
                    objective: None,
                    status: Some(crate::ThreadGoalStatus::Complete),
                    token_budget: None,
                    expected_goal_id: Some(first.goal_id),
                },
            )
            .await
            .expect("stale update should not fail")
    );
    assert_eq!(
        Some(second),
        runtime
            .thread_goals()
            .get_thread_goal(thread_id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn exact_accounting_returns_the_promoted_version() {
    let runtime = test_runtime().await;
    let thread_id = test_thread_id();
    let goal = runtime
        .thread_goals()
        .create_thread_goal_exact(
            thread_id,
            /*expected_revision*/ 0,
            "account usage",
            crate::ThreadGoalStatus::Active,
            /*token_budget*/ Some(100),
        )
        .await
        .unwrap()
        .unwrap();
    let outcome = runtime
        .thread_goals()
        .account_thread_goal_usage_exact(
            thread_id,
            /*time_delta_seconds*/ 2,
            /*token_delta*/ 3,
            GoalAccountingMode::ActiveOnly,
            &GoalVersion::from(&goal),
        )
        .await
        .unwrap();
    let GoalAccountingOutcome::Updated(updated) = outcome else {
        panic!("exact accounting should update the goal");
    };
    assert_eq!(2, updated.revision);
    assert_eq!(3, updated.tokens_used);
    assert_eq!(2, updated.time_used_seconds);
}
