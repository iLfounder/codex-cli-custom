use std::sync::RwLock;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_transport::SUPERVISOR_CONTRACT_VERSION;
use codex_app_server_transport::SupervisedAppServerSnapshot;
use codex_uds::UnixListener;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::AppServerRequestHandle;

#[derive(Clone)]
struct SnapshotFixture {
    snapshot: Arc<RwLock<SupervisorSnapshot>>,
}

impl SnapshotFixture {
    fn new(snapshot: SupervisorSnapshot) -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(snapshot)),
        }
    }

    fn set(&self, snapshot: SupervisorSnapshot) {
        *self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
    }
}

impl SupervisorSnapshotSource for SnapshotFixture {
    fn read_snapshot(&self) -> SupervisorSnapshotFuture<'_> {
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(snapshot) })
    }
}

#[derive(Clone, Debug)]
enum MockCommand {
    Close(usize),
    Respond(usize, RequestId),
}

#[derive(Debug)]
enum MockEvent {
    Initialize(usize),
    Request {
        connection: usize,
        request_id: RequestId,
        method: String,
    },
}

struct MockServer {
    command_tx: broadcast::Sender<MockCommand>,
    event_rx: mpsc::UnboundedReceiver<MockEvent>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(socket_path: &AbsolutePathBuf, init_gate: watch::Receiver<bool>) -> Self {
        let mut listener = UnixListener::bind(socket_path.as_path())
            .await
            .expect("listener should bind");
        let (command_tx, _) = broadcast::channel(16);
        let connection_command_tx = command_tx.clone();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let accept_task = tokio::spawn(async move {
            let mut connection = 0;
            loop {
                let Ok(stream) = listener.accept().await else {
                    break;
                };
                let command_rx = connection_command_tx.subscribe();
                let event_tx = event_tx.clone();
                let init_gate = init_gate.clone();
                tokio::spawn(run_mock_connection(
                    connection, stream, command_rx, event_tx, init_gate,
                ));
                connection += 1;
            }
        });
        Self {
            command_tx,
            event_rx,
            accept_task,
        }
    }

    fn command(&self, command: MockCommand) {
        self.command_tx
            .send(command)
            .expect("mock connection should receive command");
    }

    async fn next_event(&mut self) -> MockEvent {
        timeout(Duration::from_secs(2), self.event_rx.recv())
            .await
            .expect("mock event should arrive")
            .expect("mock event stream should stay open")
    }

    async fn next_request(&mut self) -> (usize, RequestId, String) {
        loop {
            if let MockEvent::Request {
                connection,
                request_id,
                method,
            } = self.next_event().await
            {
                return (connection, request_id, method);
            }
        }
    }

    async fn next_initialize(&mut self) -> usize {
        loop {
            if let MockEvent::Initialize(connection) = self.next_event().await {
                return connection;
            }
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

async fn run_mock_connection(
    connection: usize,
    stream: codex_uds::UnixStream,
    mut command_rx: broadcast::Receiver<MockCommand>,
    event_tx: mpsc::UnboundedSender<MockEvent>,
    mut init_gate: watch::Receiver<bool>,
) {
    let mut websocket = accept_async(stream)
        .await
        .expect("websocket upgrade should succeed");
    let JSONRPCMessage::Request(initialize) = read_message(&mut websocket).await else {
        panic!("expected initialize request");
    };
    assert_eq!(initialize.method, "initialize");
    event_tx
        .send(MockEvent::Initialize(connection))
        .expect("initialize event should send");
    while !*init_gate.borrow() {
        init_gate
            .changed()
            .await
            .expect("initialize gate should stay open");
    }
    write_message(
        &mut websocket,
        JSONRPCMessage::Response(JSONRPCResponse {
            id: initialize.id,
            result: serde_json::json!({
                "userAgent": "codex-test/0.0.0",
                "codexHome": "/test/.codex",
            }),
        }),
    )
    .await;
    let JSONRPCMessage::Notification(initialized) = read_message(&mut websocket).await else {
        panic!("expected initialized notification");
    };
    assert_eq!(initialized.method, "initialized");
    loop {
        tokio::select! {
            message = websocket.next() => {
                let Some(Ok(Message::Text(text))) = message else {
                    break;
                };
                match serde_json::from_str::<JSONRPCMessage>(&text)
                    .expect("client message should be JSON-RPC")
                {
                    JSONRPCMessage::Request(request) => {
                        event_tx.send(MockEvent::Request {
                            connection,
                            request_id: request.id,
                            method: request.method,
                        }).expect("request event should send");
                    }
                    JSONRPCMessage::Notification(_)
                    | JSONRPCMessage::Response(_)
                    | JSONRPCMessage::Error(_) => {}
                }
            }
            command = command_rx.recv() => {
                match command {
                    Ok(MockCommand::Close(target)) if target == connection => {
                        websocket.close(None).await.expect("close should succeed");
                        break;
                    }
                    Ok(MockCommand::Respond(target, request_id)) if target == connection => {
                        write_message(
                            &mut websocket,
                            JSONRPCMessage::Response(JSONRPCResponse {
                                id: request_id,
                                result: serde_json::json!({"ok": true}),
                            }),
                        ).await;
                    }
                    Ok(MockCommand::Close(_) | MockCommand::Respond(_, _)) => {}
                    Err(_) => break,
                }
            }
        }
    }
}

async fn read_message<S>(websocket: &mut tokio_tungstenite::WebSocketStream<S>) -> JSONRPCMessage
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let message = websocket
            .next()
            .await
            .expect("websocket message should arrive")
            .expect("websocket message should succeed");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("message should be JSON-RPC");
        }
    }
}

async fn write_message<S>(
    websocket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: JSONRPCMessage,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    websocket
        .send(Message::Text(
            serde_json::to_string(&message)
                .expect("message should serialize")
                .into(),
        ))
        .await
        .expect("message should send");
}

fn identity(discriminator: u128, generation: u64) -> AppServerInstanceIdentity {
    AppServerInstanceIdentity {
        instance_id: format!("00000000-0000-0000-0000-{discriminator:012x}")
            .parse()
            .expect("identity should parse"),
        generation,
    }
}

fn ready_snapshot(
    snapshot_revision: u64,
    instance: AppServerInstanceIdentity,
    predecessor: Option<AppServerInstanceIdentity>,
) -> SupervisorSnapshot {
    SupervisorSnapshot {
        contract_version: SUPERVISOR_CONTRACT_VERSION,
        snapshot_revision,
        app_server: Some(SupervisedAppServerSnapshot {
            process_generation: snapshot_revision + 100,
            instance,
            predecessor,
            status: SupervisedAppServerStatus::Ready,
        }),
    }
}

fn connect_args() -> SupervisedAppServerConnectArgs {
    SupervisedAppServerConnectArgs {
        client_name: "supervised-client-test".to_string(),
        client_version: "0.0.0-test".to_string(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 8,
    }
}

fn diagnostics_request(id: i64) -> ClientRequest {
    ClientRequest::ServerDiagnostics {
        request_id: RequestId::Integer(id),
        params: Default::default(),
    }
}

async fn wait_connected(
    client: &mut SupervisedAppServerClient,
    expected: AppServerInstanceIdentity,
) {
    loop {
        if let Some(SupervisedAppServerEvent::Connected { identity }) =
            timeout(Duration::from_secs(2), client.next_event())
                .await
                .expect("connection event should arrive")
        {
            assert_eq!(identity, expected);
            return;
        }
    }
}

#[tokio::test]
async fn requests_fail_fast_until_initialize_is_published() {
    let temp = TempDir::new().expect("temp dir");
    let socket_path = AbsolutePathBuf::from_absolute_path(temp.path().join("app.sock"))
        .expect("socket path should resolve");
    let (init_gate_tx, init_gate_rx) = watch::channel(false);
    let mut server = MockServer::start(&socket_path, init_gate_rx).await;
    let expected = identity(1, 1);
    let snapshots = SnapshotFixture::new(ready_snapshot(1, expected, None));
    let mut client =
        SupervisedAppServerClient::start_at(connect_args(), socket_path, Arc::new(snapshots));

    assert_eq!(server.next_initialize().await, 0);
    let error = client
        .request(diagnostics_request(1))
        .await
        .expect_err("request must fail before initialize completes");
    assert_eq!(error.kind(), ErrorKind::NotConnected);
    assert_eq!(client.connected_identity(), None);

    init_gate_tx.send(true).expect("initialize should release");
    wait_connected(&mut client, expected).await;
    assert_eq!(client.connected_identity(), Some(expected));
    client.shutdown().await;
}

#[tokio::test]
async fn pending_request_fails_once_without_replay_and_clones_use_replacement() {
    let temp = TempDir::new().expect("temp dir");
    let socket_path = AbsolutePathBuf::from_absolute_path(temp.path().join("app.sock"))
        .expect("socket path should resolve");
    let (_init_gate_tx, init_gate_rx) = watch::channel(true);
    let mut server = MockServer::start(&socket_path, init_gate_rx).await;
    let first = identity(1, 1);
    let second = identity(2, 2);
    let snapshots = SnapshotFixture::new(ready_snapshot(1, first, None));
    let mut client = SupervisedAppServerClient::start_at(
        connect_args(),
        socket_path,
        Arc::new(snapshots.clone()),
    );
    let first_handle = AppServerRequestHandle::Supervised(client.request_handle());
    let second_handle = first_handle.clone();
    wait_connected(&mut client, first).await;

    let pending = tokio::spawn(async move { first_handle.request(diagnostics_request(11)).await });
    let (connection, request_id, _) = server.next_request().await;
    assert_eq!(
        (connection, request_id.clone()),
        (0, RequestId::Integer(11))
    );
    snapshots.set(ready_snapshot(2, second, Some(first)));
    server.command(MockCommand::Close(0));
    let error = pending
        .await
        .expect("pending request task should join")
        .expect_err("disconnected pending request should fail");
    assert_ne!(error.kind(), ErrorKind::NotConnected);
    wait_connected(&mut client, second).await;
    let replacement =
        tokio::spawn(async move { second_handle.request(diagnostics_request(12)).await });
    let (connection, request_id, _) = server.next_request().await;
    assert_eq!(
        (connection, request_id.clone()),
        (1, RequestId::Integer(12))
    );
    server.command(MockCommand::Respond(connection, request_id));
    assert_eq!(
        replacement
            .await
            .expect("replacement request task should join")
            .expect("replacement request should complete"),
        Ok(serde_json::json!({"ok": true}))
    );
    client.shutdown().await;
}

#[tokio::test]
async fn reconnect_rejects_same_or_lower_generation_but_allows_skipped_predecessor() {
    let temp = TempDir::new().expect("temp dir");
    let socket_path = AbsolutePathBuf::from_absolute_path(temp.path().join("app.sock"))
        .expect("socket path should resolve");
    let (_init_gate_tx, init_gate_rx) = watch::channel(true);
    let mut server = MockServer::start(&socket_path, init_gate_rx).await;
    let first = identity(1, 3);
    let snapshots = SnapshotFixture::new(ready_snapshot(1, first, None));
    let mut client = SupervisedAppServerClient::start_at(
        connect_args(),
        socket_path,
        Arc::new(snapshots.clone()),
    );
    assert_eq!(server.next_initialize().await, 0);
    wait_connected(&mut client, first).await;
    snapshots.set(ready_snapshot(2, identity(2, 3), None));
    server.command(MockCommand::Close(0));
    loop {
        if matches!(
            client.next_event().await,
            Some(SupervisedAppServerEvent::AppServer(
                AppServerEvent::Disconnected { .. }
            ))
        ) {
            break;
        }
    }
    assert_eq!(
        client
            .request(diagnostics_request(20))
            .await
            .expect_err("outage request must fail immediately")
            .kind(),
        ErrorKind::NotConnected
    );
    assert!(
        timeout(Duration::from_millis(150), server.event_rx.recv())
            .await
            .is_err()
    );
    snapshots.set(ready_snapshot(3, identity(3, 2), None));
    assert!(
        timeout(Duration::from_millis(150), server.event_rx.recv())
            .await
            .is_err()
    );

    let skipped = identity(5, 5);
    snapshots.set(ready_snapshot(4, skipped, Some(identity(4, 4))));
    assert_eq!(server.next_initialize().await, 1);
    wait_connected(&mut client, skipped).await;
    assert_eq!(client.connected_identity(), Some(skipped));
    client.shutdown().await;
}

#[tokio::test]
async fn transient_disconnect_reconnects_the_exact_same_ready_identity_without_replay() {
    let temp = TempDir::new().expect("temp dir");
    let socket_path = AbsolutePathBuf::from_absolute_path(temp.path().join("app.sock"))
        .expect("socket path should resolve");
    let (_init_gate_tx, init_gate_rx) = watch::channel(true);
    let mut server = MockServer::start(&socket_path, init_gate_rx).await;
    let expected = identity(1, 7);
    let snapshots = SnapshotFixture::new(ready_snapshot(11, expected, None));
    let mut client =
        SupervisedAppServerClient::start_at(connect_args(), socket_path, Arc::new(snapshots));
    wait_connected(&mut client, expected).await;

    let handle = client.request_handle();
    let pending = tokio::spawn(async move { handle.request(diagnostics_request(31)).await });
    let (connection, request_id, _) = server.next_request().await;
    assert_eq!((connection, request_id), (0, RequestId::Integer(31)));
    server.command(MockCommand::Close(0));
    pending
        .await
        .expect("pending request task should join")
        .expect_err("disconnected request must fail rather than replay");

    assert_eq!(server.next_initialize().await, 1);
    wait_connected(&mut client, expected).await;
    let replacement = client.request_handle();
    let completed = tokio::spawn(async move { replacement.request(diagnostics_request(32)).await });
    let (connection, request_id, _) = server.next_request().await;
    assert_eq!(
        (connection, request_id.clone()),
        (1, RequestId::Integer(32))
    );
    server.command(MockCommand::Respond(connection, request_id));
    assert_eq!(
        completed
            .await
            .expect("replacement request task should join")
            .expect("replacement request should complete"),
        Ok(serde_json::json!({"ok": true}))
    );
    client.shutdown().await;
}
