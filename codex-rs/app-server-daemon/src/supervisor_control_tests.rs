use std::os::unix::fs::PermissionsExt;

use codex_app_server_transport::AppServerInstanceIdentity;
use codex_app_server_transport::SUPERVISOR_CONTRACT_VERSION;
use codex_app_server_transport::SupervisedAppServerSnapshot;
use codex_app_server_transport::SupervisedAppServerStatus;
use codex_app_server_transport::SupervisorControlErrorCode;
use codex_app_server_transport::SupervisorControlMessage;
use codex_app_server_transport::SupervisorControlRequest;
use codex_app_server_transport::SupervisorSnapshot;
use codex_uds::UnixStream;
use pretty_assertions::assert_eq;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::sync::watch;
use uuid::Uuid;

use super::ControlCommand;
use super::ControlServer;
use super::Frame;
use super::MAX_FRAME_BYTES;
use super::read_frame;
use super::restart_error_for_snapshot;

fn identity(id: &str, generation: u64) -> AppServerInstanceIdentity {
    AppServerInstanceIdentity {
        instance_id: Uuid::parse_str(id).expect("valid test UUID"),
        generation,
    }
}

fn ready_snapshot(instance: AppServerInstanceIdentity, revision: u64) -> SupervisorSnapshot {
    SupervisorSnapshot {
        contract_version: SUPERVISOR_CONTRACT_VERSION,
        snapshot_revision: revision,
        app_server: Some(SupervisedAppServerSnapshot {
            process_generation: revision,
            instance,
            predecessor: None,
            status: SupervisedAppServerStatus::Ready,
        }),
    }
}

async fn send_request(
    writer: &mut tokio::io::WriteHalf<UnixStream>,
    request: &SupervisorControlRequest,
) {
    writer
        .write_all(&serde_json::to_vec(request).expect("serialize request"))
        .await
        .expect("write request");
    writer.write_all(b"\n").await.expect("write newline");
    writer.flush().await.expect("flush request");
}

async fn read_message(
    reader: &mut BufReader<tokio::io::ReadHalf<UnixStream>>,
) -> SupervisorControlMessage {
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("read response");
    serde_json::from_str(&line).expect("deserialize response")
}

#[tokio::test]
async fn control_socket_is_private_and_publishes_snapshot_updates() {
    let temp = tempfile::tempdir().expect("temp dir");
    let socket = temp.path().join("control/supervisor.sock");
    let initial = ready_snapshot(identity("11111111-1111-4111-8111-111111111111", 1), 1);
    let (updates, snapshots) = watch::channel(initial.clone());
    let (commands, _command_rx) = mpsc::channel(1);
    let server = ControlServer::start(socket.clone(), snapshots, commands)
        .await
        .expect("start control server");

    assert_eq!(
        tokio::fs::metadata(socket.parent().expect("socket parent"))
            .await
            .expect("parent metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        tokio::fs::metadata(&socket)
            .await
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let stream = UnixStream::connect(&socket).await.expect("connect control");
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    send_request(
        &mut writer,
        &SupervisorControlRequest::SnapshotRead { id: 1 },
    )
    .await;
    assert_eq!(
        read_message(&mut reader).await,
        SupervisorControlMessage::Response(codex_app_server_transport::SupervisorControlResponse {
            id: 1,
            snapshot: initial,
        })
    );

    let updated = ready_snapshot(identity("22222222-2222-4222-8222-222222222222", 2), 2);
    updates.send_replace(updated.clone());
    assert_eq!(
        read_message(&mut reader).await,
        SupervisorControlMessage::Event(codex_app_server_transport::SupervisorControlEvent {
            event: codex_app_server_transport::SupervisorControlEventMethod::SnapshotUpdated,
            snapshot: updated,
        })
    );

    server.shutdown().await;
    assert!(!socket.exists());
}

#[tokio::test]
async fn restart_requires_exact_ready_instance_and_returns_replacement() {
    let temp = tempfile::tempdir().expect("temp dir");
    let socket = temp.path().join("control/supervisor.sock");
    let current = identity("11111111-1111-4111-8111-111111111111", 1);
    let replacement = ready_snapshot(identity("22222222-2222-4222-8222-222222222222", 2), 2);
    let (_updates, snapshots) = watch::channel(ready_snapshot(current, 1));
    let (commands, mut command_rx) = mpsc::channel(1);
    let server = ControlServer::start(socket.clone(), snapshots, commands)
        .await
        .expect("start control server");
    let stream = UnixStream::connect(&socket).await.expect("connect control");
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    send_request(
        &mut writer,
        &SupervisorControlRequest::AppServerRestart {
            id: 1,
            expected_instance: identity("33333333-3333-4333-8333-333333333333", 3),
        },
    )
    .await;
    let SupervisorControlMessage::Error(error) = read_message(&mut reader).await else {
        panic!("expected stale error");
    };
    assert_eq!(error.error.code, SupervisorControlErrorCode::StaleInstance);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    send_request(
        &mut writer,
        &SupervisorControlRequest::AppServerRestart {
            id: 2,
            expected_instance: current,
        },
    )
    .await;
    let command = command_rx.recv().await.expect("restart command");
    let ControlCommand::Restart { expected, reply } = command;
    assert_eq!(expected, current);
    reply.send(Ok(replacement.clone())).expect("reply restart");
    assert_eq!(
        read_message(&mut reader).await,
        SupervisorControlMessage::Response(codex_app_server_transport::SupervisorControlResponse {
            id: 2,
            snapshot: replacement,
        })
    );

    server.shutdown().await;
}

#[test]
fn restart_admission_fails_closed_until_exact_instance_is_ready() {
    let current = identity("11111111-1111-4111-8111-111111111111", 1);
    let stale = identity("22222222-2222-4222-8222-222222222222", 2);
    let mut snapshot = SupervisorSnapshot {
        contract_version: SUPERVISOR_CONTRACT_VERSION,
        snapshot_revision: 1,
        app_server: None,
    };
    assert_eq!(
        restart_error_for_snapshot(&snapshot, current),
        Some(SupervisorControlErrorCode::NoCurrentInstance)
    );
    snapshot.app_server = Some(SupervisedAppServerSnapshot {
        process_generation: 1,
        instance: current,
        predecessor: None,
        status: SupervisedAppServerStatus::Starting,
    });
    assert_eq!(
        restart_error_for_snapshot(&snapshot, current),
        Some(SupervisorControlErrorCode::NotReady)
    );
    snapshot.app_server.as_mut().expect("app-server").status = SupervisedAppServerStatus::Backoff;
    assert_eq!(
        restart_error_for_snapshot(&snapshot, current),
        Some(SupervisorControlErrorCode::NotReady)
    );
    snapshot.app_server.as_mut().expect("app-server").status = SupervisedAppServerStatus::Ready;
    assert_eq!(
        restart_error_for_snapshot(&snapshot, stale),
        Some(SupervisorControlErrorCode::StaleInstance)
    );
    assert_eq!(restart_error_for_snapshot(&snapshot, current), None);
}

#[tokio::test]
async fn control_server_reclaims_stale_socket_and_rejects_second_owner() {
    let temp = tempfile::tempdir().expect("temp dir");
    let control_dir = temp.path().join("control");
    std::fs::create_dir(&control_dir).expect("create control dir");
    let socket = control_dir.join("supervisor.sock");
    drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind stale socket"));
    let snapshot = SupervisorSnapshot {
        contract_version: SUPERVISOR_CONTRACT_VERSION,
        snapshot_revision: 1,
        app_server: None,
    };
    let (_updates, snapshots) = watch::channel(snapshot.clone());
    let (commands, _command_rx) = mpsc::channel(1);
    let server = ControlServer::start(socket.clone(), snapshots, commands)
        .await
        .expect("reclaim stale socket");
    let (_other_updates, other_snapshots) = watch::channel(snapshot);
    let (other_commands, _other_command_rx) = mpsc::channel(1);

    assert!(
        ControlServer::start(socket.clone(), other_snapshots, other_commands)
            .await
            .is_err()
    );

    server.shutdown().await;
}

#[tokio::test]
async fn oversized_frame_is_rejected_before_deserialization() {
    let (client, server) = tokio::io::duplex(MAX_FRAME_BYTES + 8);
    let writer = tokio::spawn(async move {
        let mut client = client;
        client
            .write_all(&vec![b'x'; MAX_FRAME_BYTES + 1])
            .await
            .expect("write oversized payload");
        client.write_all(b"\n").await.expect("write newline");
    });
    let mut reader = BufReader::new(server);

    assert!(matches!(
        read_frame(&mut reader).await.expect("read frame"),
        Some(Frame::TooLarge)
    ));
    writer.await.expect("writer task");
}
