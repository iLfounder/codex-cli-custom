use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use uuid::Uuid;

use super::AppServerInstanceIdentity;
use super::CanonicalControlPaths;
use super::SupervisedAppServerSnapshot;
use super::SupervisedAppServerStatus;
use super::SupervisorControlError;
use super::SupervisorControlErrorCode;
use super::SupervisorControlErrorResponse;
use super::SupervisorControlEvent;
use super::SupervisorControlEventMethod;
use super::SupervisorControlMessage;
use super::SupervisorControlRequest;
use super::SupervisorControlResponse;
use super::SupervisorSnapshot;
use super::ready_proof_matches_at;
use super::remove_ready_proof;
use super::write_supervised_ready_identity;

#[test]
fn owner_control_paths_are_fixed_outside_numbered_codex_homes() {
    let owner_home = tempfile::tempdir().expect("owner home");
    let account_one = owner_home.path().join(".codex/account1");
    let account_ten = owner_home.path().join(".codex/account10");
    let paths = CanonicalControlPaths::from_owner_home(owner_home.path()).expect("control paths");

    assert_eq!(
        paths.root().as_path(),
        owner_home.path().join(".codex/app-server-control")
    );
    assert_eq!(
        paths.app_server_socket().as_path(),
        owner_home
            .path()
            .join(".codex/app-server-control/app-server-control.sock")
    );
    assert!(!paths.app_server_socket().as_path().starts_with(account_one));
    assert!(!paths.app_server_socket().as_path().starts_with(account_ten));
    assert_eq!(
        paths.supervisor_socket().as_path(),
        owner_home
            .path()
            .join(".codex/app-server-control/supervisor.sock")
    );
    assert_eq!(
        paths.supervisor_snapshot().as_path(),
        owner_home
            .path()
            .join(".codex/app-server-control/supervisor-snapshot.json")
    );
}

#[test]
fn supervisor_snapshot_keeps_revision_domains_separate() {
    let predecessor = AppServerInstanceIdentity {
        instance_id: Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .expect("predecessor UUID"),
        generation: 4,
    };
    let current = AppServerInstanceIdentity {
        instance_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222").expect("current UUID"),
        generation: 7,
    };
    let snapshot = SupervisorSnapshot {
        snapshot_revision: 19,
        app_server: Some(SupervisedAppServerSnapshot {
            process_generation: 11,
            instance: current,
            predecessor: Some(predecessor),
            status: SupervisedAppServerStatus::Ready,
        }),
    };

    assert_eq!(
        serde_json::to_value(&snapshot).expect("serialize snapshot"),
        json!({
            "snapshotRevision": 19,
            "appServer": {
                "processGeneration": 11,
                "instance": {
                    "instanceId": "22222222-2222-4222-8222-222222222222",
                    "generation": 7
                },
                "predecessor": {
                    "instanceId": "11111111-1111-4111-8111-111111111111",
                    "generation": 4
                },
                "status": "ready"
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<SupervisorSnapshot>(
            serde_json::to_value(&snapshot).expect("serialize snapshot")
        )
        .expect("deserialize snapshot"),
        snapshot
    );
}

#[test]
fn supervisor_control_wire_is_bounded_to_snapshot_and_exact_restart() {
    let instance = AppServerInstanceIdentity {
        instance_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .expect("instance UUID"),
        generation: 7,
    };
    let snapshot = SupervisorSnapshot {
        snapshot_revision: 19,
        app_server: Some(SupervisedAppServerSnapshot {
            process_generation: 11,
            instance,
            predecessor: None,
            status: SupervisedAppServerStatus::Ready,
        }),
    };
    let messages = vec![
        SupervisorControlMessage::Response(SupervisorControlResponse {
            id: 1,
            snapshot: snapshot.clone(),
        }),
        SupervisorControlMessage::Error(SupervisorControlErrorResponse {
            id: Some(2),
            error: SupervisorControlError {
                code: SupervisorControlErrorCode::StaleInstance,
                message: "app-server instance identity is stale".to_string(),
            },
        }),
        SupervisorControlMessage::Event(SupervisorControlEvent {
            event: SupervisorControlEventMethod::SnapshotUpdated,
            snapshot,
        }),
    ];

    assert_eq!(
        serde_json::to_value(SupervisorControlRequest::SnapshotRead { id: 1 })
            .expect("serialize read"),
        json!({"id": 1, "method": "snapshot/read"})
    );
    assert_eq!(
        serde_json::to_value(SupervisorControlRequest::AppServerRestart {
            id: 2,
            expected_instance: instance,
        })
        .expect("serialize restart"),
        json!({
            "id": 2,
            "method": "appServer/restart",
            "expectedInstance": {
                "instanceId": "22222222-2222-4222-8222-222222222222",
                "generation": 7
            }
        })
    );
    assert_eq!(
        messages
            .iter()
            .map(|message| {
                serde_json::from_value::<SupervisorControlMessage>(
                    serde_json::to_value(message).expect("serialize message"),
                )
                .expect("deserialize message")
            })
            .collect::<Vec<_>>(),
        messages
    );
}

#[tokio::test]
async fn ready_proof_is_private_and_removed_with_its_guard() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("app-server-ready.json");
    let absolute = AbsolutePathBuf::from_absolute_path(&path).expect("absolute proof path");
    let identity = AppServerInstanceIdentity {
        instance_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .expect("instance UUID"),
        generation: 7,
    };

    let guard = write_supervised_ready_identity(absolute, identity)
        .await
        .expect("write proof");

    assert_eq!(
        serde_json::from_slice::<AppServerInstanceIdentity>(
            &tokio::fs::read(&path).await.expect("read proof")
        )
        .expect("deserialize proof"),
        identity
    );
    assert_eq!(
        tokio::fs::metadata(&path)
            .await
            .expect("proof metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    drop(guard);
    assert!(!path.exists());
}

#[tokio::test]
async fn readiness_rejects_missing_and_stale_proofs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("ready.json");
    let expected = AppServerInstanceIdentity {
        instance_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .expect("expected UUID"),
        generation: 7,
    };
    let stale = AppServerInstanceIdentity {
        instance_id: Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("stale UUID"),
        generation: 6,
    };

    assert!(
        !ready_proof_matches_at(&path, expected)
            .await
            .expect("missing proof")
    );
    tokio::fs::write(
        &path,
        serde_json::to_vec(&stale).expect("serialize stale proof"),
    )
    .await
    .expect("write stale proof");
    assert!(
        !ready_proof_matches_at(&path, expected)
            .await
            .expect("stale proof")
    );
    tokio::fs::write(
        &path,
        serde_json::to_vec(&expected).expect("serialize expected proof"),
    )
    .await
    .expect("write expected proof");
    assert!(
        ready_proof_matches_at(&path, expected)
            .await
            .expect("expected proof")
    );

    remove_ready_proof(&path).await.expect("invalidate proof");
    assert!(!path.exists());
}
