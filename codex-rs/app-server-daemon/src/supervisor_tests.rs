use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use codex_app_server_transport::AppServerInstanceIdentity;
use codex_app_server_transport::SUPERVISOR_CONTRACT_VERSION;
use codex_app_server_transport::SupervisedAppServerSnapshot;
use codex_app_server_transport::SupervisedAppServerStatus;
use codex_app_server_transport::SupervisorSnapshot;
use pretty_assertions::assert_eq;
use tokio::process::Command;
use tokio::time::Instant;
use uuid::Uuid;

use super::BACKOFF_MAX;
use super::Candidate;
use super::ManagedChild;
use super::SnapshotPublisher;
use super::SupervisorSeed;
use super::backoff_delay;
use super::process_start_identity;
use super::read_supervisor_seed;
use super::stop_exact;
use super::validate_supervisor_codex_home;

fn identity(id: &str, generation: u64) -> AppServerInstanceIdentity {
    AppServerInstanceIdentity {
        instance_id: Uuid::parse_str(id).expect("valid test UUID"),
        generation,
    }
}

#[tokio::test]
async fn snapshot_publication_is_private_revisioned_and_sanitized() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("snapshot.json");
    let predecessor = identity("11111111-1111-4111-8111-111111111111", 6);
    let current = identity("22222222-2222-4222-8222-222222222222", 7);
    let projected = SupervisedAppServerSnapshot {
        process_generation: 9,
        instance: current,
        predecessor: Some(predecessor),
        status: SupervisedAppServerStatus::Starting,
    };
    let (updates, _snapshots) = tokio::sync::watch::channel(SupervisorSnapshot {
        contract_version: SUPERVISOR_CONTRACT_VERSION,
        snapshot_revision: 0,
        app_server: None,
    });
    let mut publisher = SnapshotPublisher {
        snapshot: SupervisorSnapshot {
            contract_version: SUPERVISOR_CONTRACT_VERSION,
            snapshot_revision: 0,
            app_server: None,
        },
        path: path.clone(),
        updates,
    };

    publisher
        .publish(Some(projected.clone()))
        .await
        .expect("publish snapshot");

    let contents = tokio::fs::read(&path).await.expect("read snapshot");
    let actual: SupervisorSnapshot =
        serde_json::from_slice(&contents).expect("deserialize snapshot");
    assert_eq!(
        actual,
        SupervisorSnapshot {
            contract_version: SUPERVISOR_CONTRACT_VERSION,
            snapshot_revision: 1,
            app_server: Some(projected),
        }
    );
    assert_eq!(
        tokio::fs::metadata(&path)
            .await
            .expect("snapshot metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let encoded = String::from_utf8(contents).expect("snapshot UTF-8");
    for forbidden in ["pid", "path", "credential", "account"] {
        assert!(!encoded.to_ascii_lowercase().contains(forbidden));
    }
}

#[test]
fn replacement_backoff_is_exponential_and_bounded() {
    assert_eq!(
        (1..=10).map(backoff_delay).collect::<Vec<_>>(),
        vec![
            Duration::from_millis(250),
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            BACKOFF_MAX,
            BACKOFF_MAX,
            BACKOFF_MAX,
        ]
    );
}

#[tokio::test]
async fn stop_targets_the_recorded_process_start_identity() {
    let process = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn sleep");
    let pid = process.id().expect("sleep pid");
    let process_identity = process_start_identity(pid)
        .await
        .expect("record process start identity");
    let mut child = ManagedChild {
        candidate: Candidate {
            process_generation: 1,
            identity: identity("22222222-2222-4222-8222-222222222222", 1),
        },
        process,
        process_identity,
        ready_at: Instant::now(),
    };

    stop_exact(&mut child).await.expect("stop exact child");

    assert!(child.process.try_wait().expect("child status").is_some());
}

#[test]
fn private_snapshot_resumes_all_monotonic_high_water_marks() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("snapshot.json");
    let predecessor = identity("11111111-1111-4111-8111-111111111111", 6);
    let current = identity("22222222-2222-4222-8222-222222222222", 7);
    let snapshot = SupervisorSnapshot {
        contract_version: SUPERVISOR_CONTRACT_VERSION,
        snapshot_revision: 41,
        app_server: Some(SupervisedAppServerSnapshot {
            process_generation: 12,
            instance: current,
            predecessor: Some(predecessor),
            status: SupervisedAppServerStatus::Ready,
        }),
    };
    std::fs::write(
        &path,
        serde_json::to_vec(&snapshot).expect("serialize snapshot"),
    )
    .expect("write snapshot");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("make snapshot private");

    let seed = read_supervisor_seed(&path).expect("read private snapshot");

    assert_eq!(seed.snapshot, snapshot);
    assert_eq!(seed.last_ready, Some(current));
    assert_eq!(seed.next_process_generation, 12);
    assert_eq!(seed.next_instance_generation, 7);
}

#[test]
fn non_private_or_counterless_snapshot_is_rejected_instead_of_regressing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("snapshot.json");
    let snapshot = SupervisorSnapshot {
        contract_version: SUPERVISOR_CONTRACT_VERSION,
        snapshot_revision: 41,
        app_server: Some(SupervisedAppServerSnapshot {
            process_generation: 12,
            instance: identity("22222222-2222-4222-8222-222222222222", 7),
            predecessor: None,
            status: SupervisedAppServerStatus::Backoff,
        }),
    };
    std::fs::write(
        &path,
        serde_json::to_vec(&snapshot).expect("serialize snapshot"),
    )
    .expect("write snapshot");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("make snapshot unsafe");
    assert!(read_supervisor_seed(&path).is_err());

    assert!(
        SupervisorSeed::from_snapshot(SupervisorSnapshot {
            contract_version: SUPERVISOR_CONTRACT_VERSION,
            snapshot_revision: 42,
            app_server: None,
        })
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn supervisor_requires_registered_c1_codex_home() {
    let owner = tempfile::tempdir().expect("owner home");
    let config = owner.path().join(".config");
    let c1 = owner.path().join(".codex/account1");
    let c2 = owner.path().join(".codex/account2");
    std::fs::create_dir_all(&config).expect("create config");
    std::fs::create_dir_all(&c1).expect("create C1");
    std::fs::create_dir_all(&c2).expect("create C2");
    std::fs::write(
        config.join("codex-accounts.tsv"),
        format!("1\t{}\n2\t{}\n", c1.display(), c2.display()),
    )
    .expect("write catalog");
    std::fs::set_permissions(
        config.join("codex-accounts.tsv"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("protect catalog");

    validate_supervisor_codex_home(&c1, owner.path()).expect("C1 should be accepted");
    assert!(validate_supervisor_codex_home(&c2, owner.path()).is_err());
    assert!(validate_supervisor_codex_home(&owner.path().join(".codex"), owner.path()).is_err());
}

#[cfg(unix)]
#[test]
fn supervisor_rejects_invalid_catalog_instead_of_falling_back() {
    let owner = tempfile::tempdir().expect("owner home");
    let c1 = owner.path().join(".codex/account1");
    std::fs::create_dir_all(owner.path().join(".config")).expect("create config");
    std::fs::create_dir_all(&c1).expect("create C1");
    std::fs::write(
        owner.path().join(".config/codex-accounts.tsv"),
        format!("1\t{}\n", c1.display()),
    )
    .expect("write catalog");
    std::fs::set_permissions(
        owner.path().join(".config/codex-accounts.tsv"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("make catalog unsafe");

    assert!(validate_supervisor_codex_home(&c1, owner.path()).is_err());
}
