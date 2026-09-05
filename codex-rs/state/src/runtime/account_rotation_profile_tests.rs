use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use super::AccountRotationProfile;
use super::AccountRotationProfileUpdate;
use super::ThreadAccountRotationMode;
use crate::SqliteConfig;
use crate::StateRuntime;

async fn runtime() -> (std::path::PathBuf, std::sync::Arc<StateRuntime>) {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("create sqlite home");
    let sqlite = SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("initialize runtime");
    (sqlite_home, runtime)
}

fn round_robin() -> AccountRotationProfileUpdate {
    AccountRotationProfileUpdate {
        mode: ThreadAccountRotationMode::RoundRobin,
        fixed_account_slot_id: Some("C1".to_string()),
        automatic_account_slot_ids: vec!["C1".to_string(), "C2".to_string()],
    }
}

#[tokio::test]
async fn global_profile_is_revision_zero_until_activated_and_does_not_fan_out() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let thread_id = ThreadId::default();
    let override_profile = runtime
        .compare_and_swap_thread_account_rotation_override(thread_id, 0, &round_robin())
        .await
        .expect("create override")
        .expect("revision zero override");

    assert_eq!(
        runtime
            .account_rotation_global_profile()
            .await
            .expect("read pre-activation global profile"),
        None
    );
    let committed = runtime
        .compare_and_swap_account_rotation_global_profile(0, &round_robin())
        .await
        .expect("activate global profile")
        .expect("revision zero global profile");
    assert_eq!(
        committed,
        AccountRotationProfile {
            mode: ThreadAccountRotationMode::RoundRobin,
            fixed_account_slot_id: Some("C1".to_string()),
            automatic_account_slot_ids: vec!["C1".to_string(), "C2".to_string()],
            revision: 1,
        }
    );
    assert_eq!(
        runtime
            .compare_and_swap_account_rotation_global_profile(0, &round_robin())
            .await
            .expect("reject stale global revision"),
        None
    );
    assert_eq!(
        runtime
            .thread_account_rotation_override(thread_id)
            .await
            .expect("read unchanged override"),
        Some(override_profile)
    );
    runtime.close().await;
}

#[tokio::test]
async fn thread_override_reset_requires_the_exact_override_revision() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let thread_id = ThreadId::default();
    let committed = runtime
        .compare_and_swap_thread_account_rotation_override(thread_id, 0, &round_robin())
        .await
        .expect("create override")
        .expect("revision zero override");

    assert!(
        !runtime
            .reset_thread_account_rotation_override(thread_id, committed.revision + 1)
            .await
            .expect("reject stale reset")
    );
    assert!(
        runtime
            .reset_thread_account_rotation_override(thread_id, committed.revision)
            .await
            .expect("reset exact override")
    );
    assert_eq!(
        runtime
            .thread_account_rotation_override(thread_id)
            .await
            .expect("read reset override"),
        None
    );
    runtime.close().await;
}
