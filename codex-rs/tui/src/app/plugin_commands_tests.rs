use codex_app_server_protocol::PluginCommandTarget;
use codex_app_server_protocol::SessionRuntimeAccountBinding;
use codex_app_server_protocol::SessionRuntimeIdentity;
use codex_app_server_protocol::SessionRuntimeLifecycle;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimePersistence;
use codex_app_server_protocol::SessionRuntimePersistenceHealth;
use codex_app_server_protocol::SessionRuntimeWriter;
use codex_app_server_protocol::SessionRuntimeWriterState;
use pretty_assertions::assert_eq;

use super::*;
use crate::bottom_pane::slash_commands::BuiltinCommandFlags;
use crate::bottom_pane::slash_commands::SlashCommandItem;
use crate::bottom_pane::slash_commands::find_slash_command;

fn catalog_subject(
    thread_id: ThreadId,
    account_slot_id: &str,
    execution_generation: u64,
    invalidation_generation: u64,
) -> PluginCommandCatalogSubject {
    PluginCommandCatalogSubject {
        thread_id,
        instance_epoch: "epoch".to_string(),
        account_slot_id: Some(account_slot_id.to_string()),
        execution_generation: Some(execution_generation),
        cwd: PathBuf::from("/workspace"),
        invalidation_generation,
    }
}

fn projected_fixture_command() -> PluginSlashCommand {
    PluginSlashCommand {
        id: "fixture:review".to_string(),
        name: "fixture:review".to_string(),
        description: "Review the current change".to_string(),
        available: true,
        deny_reason: None,
        canonical: true,
    }
}

fn runtime_snapshot(
    thread_id: ThreadId,
    lifecycle_state: SessionRuntimeLifecycleState,
) -> SessionRuntimeSnapshot {
    SessionRuntimeSnapshot {
        thread_id: thread_id.to_string(),
        state_revision: 0,
        identity: SessionRuntimeIdentity {
            session_id: thread_id.to_string(),
            forked_from_id: None,
            parent_thread_id: None,
            name: None,
            source: "test".to_string(),
            cwd: "/workspace".to_string(),
            git_info: None,
            settings: None,
        },
        lifecycle: SessionRuntimeLifecycle {
            state: lifecycle_state,
            active_turn_id: None,
            waiting_on: Vec::new(),
            subscriber_count: 0,
            client_incarnations: Vec::new(),
            last_activity_at: None,
            unload_at: None,
        },
        writer: SessionRuntimeWriter {
            state: SessionRuntimeWriterState::None,
            store_id: None,
            writer_generation: None,
            deny_reason: None,
        },
        persistence: SessionRuntimePersistence {
            jsonl: None,
            sqlite: None,
            lag: None,
            flush_health: SessionRuntimePersistenceHealth::Unknown,
            materialize_health: SessionRuntimePersistenceHealth::Unknown,
            flushed_at: None,
            materialized_at: None,
            deny_reason: None,
        },
        account: SessionRuntimeAccountBinding {
            current: None,
            active_turn: None,
            switch_state: SessionRuntimeAccountSwitchState::Stable,
            switch_target_slot_id: None,
            deny_reason: None,
        },
        actions: Vec::new(),
        continuity: Default::default(),
    }
}

#[test]
fn projection_removes_one_backend_slash_for_render_and_dispatch() {
    let projected = project_commands(vec![PluginCommand {
        id: "fixture:review".to_string(),
        plugin_id: "fixture".to_string(),
        canonical_name: "/fixture:review".to_string(),
        short_name: Some("/fixture-review".to_string()),
        description: "Review the current change".to_string(),
        target: PluginCommandTarget::Prompt,
        available: true,
        deny_reason: None,
    }]);
    let expected = vec![
        PluginSlashCommand {
            id: "fixture:review".to_string(),
            name: "fixture:review".to_string(),
            description: "Review the current change".to_string(),
            available: true,
            deny_reason: None,
            canonical: true,
        },
        PluginSlashCommand {
            id: "fixture:review".to_string(),
            name: "fixture-review".to_string(),
            description: "Review the current change".to_string(),
            available: true,
            deny_reason: None,
            canonical: false,
        },
    ];

    assert_eq!(projected, expected);
    assert_eq!(
        find_slash_command(
            "fixture:review",
            BuiltinCommandFlags::default(),
            &[],
            &projected,
        ),
        Some(SlashCommandItem::Plugin(expected[0].clone()))
    );
    assert_eq!(
        find_slash_command(
            "fixture-review",
            BuiltinCommandFlags::default(),
            &[],
            &projected,
        ),
        Some(SlashCommandItem::Plugin(expected[1].clone()))
    );
    assert_eq!(
        projected
            .iter()
            .map(|command| format!("/{}", command.name))
            .collect::<Vec<_>>(),
        vec!["/fixture:review".to_string(), "/fixture-review".to_string(),]
    );
}

#[test]
fn identical_catalog_subject_is_one_flight_without_trailing_work() {
    let mut state = PluginCommandState::default();
    let subject = catalog_subject(
        ThreadId::new(),
        "default",
        /*execution_generation*/ 1,
        /*invalidation_generation*/ 0,
    );

    let request_generation = state
        .begin_catalog_request(subject.clone())
        .expect("the first subject starts a request");
    for _ in 1..29 {
        assert_eq!(state.begin_catalog_request(subject.clone()), None);
    }

    assert_eq!(state.flights.len(), 1);
    assert_eq!(
        state.complete_catalog_request(
            subject.thread_id,
            request_generation,
            PluginCommandRequestOutcome::Succeeded,
        ),
        PluginCommandCompletion {
            apply_result: true,
            schedule_trailing: false,
        }
    );
}

#[test]
fn invalidations_during_a_flight_keep_only_the_latest_trailing_subject() {
    let mut state = PluginCommandState::default();
    let thread_id = ThreadId::new();
    let initial = catalog_subject(
        thread_id, "slot-0", /*execution_generation*/ 0, /*invalidation_generation*/ 0,
    );
    let request_generation = state
        .begin_catalog_request(initial)
        .expect("the initial subject starts a request");
    let mut latest = None;

    for execution_generation in 1..=29 {
        state.invalidate_catalog();
        let subject = catalog_subject(
            thread_id,
            "slot-latest",
            execution_generation,
            state.invalidation_generation,
        );
        assert_eq!(state.begin_catalog_request(subject.clone()), None);
        latest = Some(subject);
    }
    let latest = latest.expect("the loop records a latest subject");

    assert_eq!(
        state.complete_catalog_request(
            thread_id,
            request_generation,
            PluginCommandRequestOutcome::Succeeded,
        ),
        PluginCommandCompletion {
            apply_result: false,
            schedule_trailing: true,
        }
    );
    let trailing_generation = state
        .begin_catalog_request(latest.clone())
        .expect("completion starts the one trailing request");
    assert_eq!(state.begin_catalog_request(latest), None);
    assert_eq!(
        state.flights[&thread_id].request_generation,
        trailing_generation
    );
}

#[test]
fn stale_a_b_a_completions_clear_exact_flights_without_overwriting_current() {
    let mut state = PluginCommandState::default();
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();
    let subject_a = catalog_subject(
        thread_a, "slot-a", /*execution_generation*/ 1, /*invalidation_generation*/ 0,
    );
    let request_a = state
        .begin_catalog_request(subject_a)
        .expect("A starts its first request");

    state.invalidate_catalog();
    let subject_b = catalog_subject(
        thread_b,
        "slot-b",
        /*execution_generation*/ 1,
        state.invalidation_generation,
    );
    let request_b = state
        .begin_catalog_request(subject_b)
        .expect("B starts independently");
    state.invalidate_catalog();
    let latest_a = catalog_subject(
        thread_a,
        "slot-a",
        /*execution_generation*/ 1,
        state.invalidation_generation,
    );
    assert_eq!(state.begin_catalog_request(latest_a.clone()), None);

    assert_eq!(
        state
            .complete_catalog_request(thread_a, request_a, PluginCommandRequestOutcome::Succeeded,),
        PluginCommandCompletion {
            apply_result: false,
            schedule_trailing: true,
        }
    );
    let current_a = state
        .begin_catalog_request(latest_a)
        .expect("returning to A starts one current request");
    assert_eq!(
        state
            .complete_catalog_request(thread_a, request_a, PluginCommandRequestOutcome::Succeeded,),
        PluginCommandCompletion::default()
    );
    assert_eq!(state.flights[&thread_a].request_generation, current_a);
    assert_eq!(
        state
            .complete_catalog_request(thread_b, request_b, PluginCommandRequestOutcome::Succeeded,),
        PluginCommandCompletion::default()
    );
    assert!(state.flights.contains_key(&thread_a));
    assert!(!state.flights.contains_key(&thread_b));
}

#[test]
fn thread_switch_completion_releases_the_old_single_flight_slot() {
    let mut state = PluginCommandState::default();
    let old_thread = ThreadId::new();
    let old_subject = catalog_subject(
        old_thread, "default", /*execution_generation*/ 1, /*invalidation_generation*/ 0,
    );
    let old_request = state
        .begin_catalog_request(old_subject)
        .expect("the old thread starts a request");

    state.invalidate_catalog();
    let new_thread = ThreadId::new();
    let new_subject = catalog_subject(
        new_thread,
        "default",
        /*execution_generation*/ 1,
        state.invalidation_generation,
    );
    state
        .begin_catalog_request(new_subject)
        .expect("the new thread starts independently");

    assert_eq!(
        state.complete_catalog_request(
            old_thread,
            old_request,
            PluginCommandRequestOutcome::Succeeded,
        ),
        PluginCommandCompletion::default()
    );
    assert!(!state.flights.contains_key(&old_thread));
    state.invalidate_catalog();
    let returned_subject = catalog_subject(
        old_thread,
        "default",
        /*execution_generation*/ 1,
        state.invalidation_generation,
    );
    assert!(state.begin_catalog_request(returned_subject).is_some());
}

#[test]
fn same_subject_refresh_and_current_error_preserve_usable_commands() {
    let mut state = PluginCommandState::default();
    let thread_id = ThreadId::new();
    let subject = catalog_subject(
        thread_id, "default", /*execution_generation*/ 1, /*invalidation_generation*/ 0,
    );
    let request_generation = state
        .begin_catalog_request(subject.clone())
        .expect("the subject starts a request");
    state.commands = vec![projected_fixture_command()];

    assert_eq!(state.begin_catalog_request(subject.clone()), None);
    assert_eq!(
        state.complete_catalog_request(
            thread_id,
            request_generation,
            PluginCommandRequestOutcome::Failed,
        ),
        PluginCommandCompletion::default()
    );
    assert_eq!(state.commands, vec![projected_fixture_command()]);
    assert_eq!(state.completed_subject, None);
    assert!(state.begin_catalog_request(subject).is_some());
}

#[test]
fn hard_invalidation_clears_projection_and_schedules_one_authoritative_request() {
    let mut state = PluginCommandState::default();
    let thread_id = ThreadId::new();
    let subject = catalog_subject(
        thread_id, "default", /*execution_generation*/ 1, /*invalidation_generation*/ 0,
    );
    let request_generation = state
        .begin_catalog_request(subject)
        .expect("the subject starts a request");
    assert!(
        state
            .complete_catalog_request(
                thread_id,
                request_generation,
                PluginCommandRequestOutcome::Succeeded,
            )
            .apply_result
    );
    state.commands = vec![projected_fixture_command()];
    let invoke_generation = state.request_generation;

    state.invalidate_catalog();

    assert!(state.commands.is_empty());
    assert_eq!(state.current_subject, None);
    assert_eq!(state.completed_subject, None);
    assert_eq!(state.request_generation, invoke_generation.wrapping_add(1));
    let refreshed = catalog_subject(
        thread_id,
        "default",
        /*execution_generation*/ 1,
        state.invalidation_generation,
    );
    assert!(state.begin_catalog_request(refreshed.clone()).is_some());
    assert_eq!(state.begin_catalog_request(refreshed), None);
}

#[test]
fn runtime_subject_cache_evicts_terminal_threads_and_stays_bounded() {
    let mut state = PluginCommandState::default();

    for lifecycle_state in [
        SessionRuntimeLifecycleState::NotLoaded,
        SessionRuntimeLifecycleState::Closing,
    ] {
        let thread_id = ThreadId::new();
        state.observe_runtime(
            "epoch".to_string(),
            &runtime_snapshot(thread_id, SessionRuntimeLifecycleState::Loaded),
        );
        assert!(state.runtime_subjects.contains_key(&thread_id));
        state.observe_runtime(
            "epoch".to_string(),
            &runtime_snapshot(thread_id, lifecycle_state),
        );
        assert!(!state.runtime_subjects.contains_key(&thread_id));
    }

    for _ in 0..=MAX_RUNTIME_SUBJECTS {
        let thread_id = ThreadId::new();
        state.observe_runtime(
            "epoch".to_string(),
            &runtime_snapshot(thread_id, SessionRuntimeLifecycleState::Loaded),
        );
    }

    assert_eq!(state.runtime_subjects.len(), MAX_RUNTIME_SUBJECTS);
}

#[test]
fn catalog_flights_stay_bounded_when_requests_never_complete() {
    let mut state = PluginCommandState::default();

    for _ in 0..MAX_CATALOG_FLIGHTS {
        state.invalidate_catalog();
        let subject = catalog_subject(
            ThreadId::new(),
            "default",
            /*execution_generation*/ 1,
            state.invalidation_generation,
        );
        assert!(state.begin_catalog_request(subject).is_some());
    }
    state.invalidate_catalog();
    let overflow = catalog_subject(
        ThreadId::new(),
        "default",
        /*execution_generation*/ 1,
        state.invalidation_generation,
    );

    assert_eq!(state.begin_catalog_request(overflow), None);
    assert_eq!(state.flights.len(), MAX_CATALOG_FLIGHTS);
}

#[test]
fn account_runtime_is_authoritative_and_terminal_state_blocks_cached_subject() {
    let thread_id = ThreadId::new();
    let mut state = PluginCommandState::default();
    state.observe_runtime(
        "cached-epoch".to_string(),
        &runtime_snapshot(thread_id, SessionRuntimeLifecycleState::Loaded),
    );
    let cached = state
        .runtime_subjects
        .get(&thread_id)
        .expect("loaded runtime is cached");
    let loaded_account_runtime = (
        "account-epoch".to_string(),
        runtime_snapshot(thread_id, SessionRuntimeLifecycleState::Loaded),
    );

    assert_eq!(
        plugin_command_runtime_subject_for_thread(
            thread_id,
            Some(&loaded_account_runtime),
            Some(cached),
        )
        .map(|runtime| runtime.instance_epoch),
        Some("account-epoch".to_string())
    );

    for lifecycle_state in [
        SessionRuntimeLifecycleState::Closing,
        SessionRuntimeLifecycleState::NotLoaded,
    ] {
        let terminal_account_runtime = (
            "terminal-epoch".to_string(),
            runtime_snapshot(thread_id, lifecycle_state),
        );
        assert_eq!(
            plugin_command_runtime_subject_for_thread(
                thread_id,
                Some(&terminal_account_runtime),
                Some(cached),
            ),
            None
        );
    }
}
