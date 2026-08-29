use anyhow::Context;
use anyhow::Result;
use app_test_support::DISABLE_PLUGIN_STARTUP_TASKS_ARG;
use app_test_support::TestAppServer;
use codex_app_server_protocol::LegacyAdmissionSealParams;
use codex_app_server_protocol::RequestId;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::process::Stdio;
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::process::ChildStderr;
use tokio::process::Command;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;

const INSTANCE_GENERATION: &str = "11111111-1111-4111-8111-111111111111";
const CUTOVER_EPOCH: &str = "cutover-epoch-1";
const INSTANCE_ENV: &str = "LLC_RELAY_CODEX_APP_SERVER_INSTANCE";

#[tokio::test]
async fn stdio_app_server_rejects_legacy_admission_even_with_relay_identity_env() -> Result<()> {
    let mut app_server = TestAppServer::builder()
        .without_auto_env()
        .with_env_overrides(&[(INSTANCE_ENV, Some(INSTANCE_GENERATION))])
        .build_initialized()
        .await?;

    let request_id = app_server
        .send_raw_request(
            "legacyAdmission/seal",
            Some(serde_json::to_value(seal_params())?),
        )
        .await?;
    let error = app_server
        .read_stream_until_error_message(RequestId::Integer(request_id))
        .await?;
    assert_eq!(error.id, RequestId::Integer(request_id));
    assert_eq!(error.error.code, -32600);
    assert!(error.error.message.contains("Relay-managed legacy local"));
    Ok(())
}

#[tokio::test]
async fn relay_managed_unix_app_server_seals_mutations_but_keeps_drain_requests_available()
-> Result<()> {
    let mut fixture = RelayLegacyFixture::start().await?;
    fixture.initialize().await?;

    let sealed = fixture
        .request(
            /*id*/ 2,
            "legacyAdmission/seal",
            json!({
                "cutoverEpoch": CUTOVER_EPOCH,
                "expectedAppServerInstanceGeneration": INSTANCE_GENERATION,
            }),
        )
        .await?;
    assert_eq!(
        sealed["result"]["admission"],
        json!({
            "cutoverEpoch": CUTOVER_EPOCH,
            "appServerInstanceGeneration": INSTANCE_GENERATION,
            "state": "drained",
            "inFlightMutationCount": 0,
        })
    );

    let thread_writer = fixture
        .request(/*id*/ 30, "thread/start", json!({}))
        .await?;
    assert_sealed_error(&thread_writer);

    let root_turn = fixture
        .request(
            /*id*/ 3,
            "turn/start",
            json!({
                "threadId": "22222222-2222-4222-8222-222222222222",
                "input": [{"type": "text", "text": "hello", "textElements": []}],
            }),
        )
        .await?;
    assert_sealed_error(&root_turn);

    let queued_mutation = fixture
        .request(
            /*id*/ 4,
            "thread/queue/add",
            json!({
                "threadId": "22222222-2222-4222-8222-222222222222",
                "input": [{"type": "text", "text": "queued", "textElements": []}],
                "clientUserMessageId": "queued-message-1",
            }),
        )
        .await?;
    assert_sealed_error(&queued_mutation);

    let read = fixture.request(/*id*/ 5, "thread/list", json!({})).await?;
    assert!(read.get("result").is_some(), "read failed: {read}");

    let cancellation = fixture
        .request(
            /*id*/ 6,
            "turn/interrupt",
            json!({
                "threadId": "22222222-2222-4222-8222-222222222222",
                "turnId": "33333333-3333-4333-8333-333333333333",
            }),
        )
        .await?;
    assert_not_sealed_error(&cancellation);

    // Client responses that could resume executable work are ignored while sealed.
    // An unknown ID remains a no-op either way.
    fixture
        .send(json!({"id": 999, "result": {"decision": "accept"}}))
        .await?;

    let stale_identity = fixture
        .request(
            /*id*/ 7,
            "legacyAdmission/status",
            json!({
                "cutoverEpoch": CUTOVER_EPOCH,
                "expectedAppServerInstanceGeneration": "replacement-instance",
            }),
        )
        .await?;
    assert!(
        stale_identity["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("generation mismatch")),
        "identity mismatch did not fail closed: {stale_identity}"
    );

    let aborted = fixture
        .request(
            /*id*/ 8,
            "legacyAdmission/abort",
            json!({
                "cutoverEpoch": CUTOVER_EPOCH,
                "expectedAppServerInstanceGeneration": INSTANCE_GENERATION,
            }),
        )
        .await?;
    assert_eq!(
        aborted["result"]["admission"]["state"],
        Value::String("aborted".to_string())
    );

    let reopened = fixture.request(/*id*/ 9, "thread/start", json!({})).await?;
    assert_not_sealed_error(&reopened);
    Ok(())
}

fn assert_sealed_error(response: &Value) {
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("legacy admission is sealed")),
        "request was not rejected by the admission seal: {response}"
    );
}

fn assert_not_sealed_error(response: &Value) {
    assert!(
        !response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("legacy admission is sealed")),
        "drain-compatible request was rejected by the seal: {response}"
    );
}

fn seal_params() -> LegacyAdmissionSealParams {
    LegacyAdmissionSealParams {
        cutover_epoch: CUTOVER_EPOCH.to_string(),
        expected_app_server_instance_generation: INSTANCE_GENERATION.to_string(),
    }
}

struct RelayLegacyFixture {
    _codex_home: TempDir,
    process: Child,
    stderr: ChildStderr,
    websocket: WebSocketStream<UnixStream>,
}

impl RelayLegacyFixture {
    async fn start() -> Result<Self> {
        let codex_home = TempDir::new()?;
        let socket_path = codex_home.path().join("legacy-app-server.sock");
        let program = codex_utils_cargo_bin::cargo_bin("codex-app-server")
            .context("resolve codex-app-server binary")?;
        let mut command = Command::new(program);
        command
            .arg("--listen")
            .arg(format!("unix://{}", socket_path.display()))
            .arg(DISABLE_PLUGIN_STARTUP_TASKS_ARG)
            .current_dir(codex_home.path())
            .env("CODEX_HOME", codex_home.path())
            .env(INSTANCE_ENV, INSTANCE_GENERATION)
            .env("CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut process = command.spawn().context("spawn Unix app-server")?;
        let stderr = process
            .stderr
            .take()
            .context("capture Unix app-server stderr")?;
        let stream = connect(&mut process, &socket_path).await?;
        let (websocket, response) = client_async("ws://localhost/rpc", stream)
            .await
            .context("upgrade Unix app-server websocket")?;
        anyhow::ensure!(
            response.status().as_u16() == 101,
            "Unix app-server websocket upgrade returned {}",
            response.status()
        );
        Ok(Self {
            _codex_home: codex_home,
            process,
            stderr,
            websocket,
        })
    }

    async fn initialize(&mut self) -> Result<()> {
        let response = self
            .request(
                /*id*/ 1,
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "legacy-admission-integration-test",
                        "version": "0.1.0",
                    },
                    "capabilities": {"experimentalApi": true},
                }),
            )
            .await?;
        assert!(
            response.get("result").is_some(),
            "initialize failed: {response}"
        );
        self.send(json!({"method": "initialized"})).await?;
        Ok(())
    }

    async fn request(&mut self, id: i64, method: &str, params: Value) -> Result<Value> {
        self.send(json!({"id": id, "method": method, "params": params}))
            .await?;
        loop {
            let message = self.read().await?;
            if message["id"] == json!(id) {
                return Ok(message);
            }
        }
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        self.websocket
            .send(Message::Text(serde_json::to_string(&message)?.into()))
            .await?;
        Ok(())
    }

    async fn read(&mut self) -> Result<Value> {
        let close_reason = loop {
            match self.websocket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(text.as_ref())
                        .context("parse app-server JSON-RPC message");
                }
                Some(Ok(Message::Close(frame))) => break format!("close frame: {frame:?}"),
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(error).context("read app-server websocket"),
                None => break "websocket stream ended".to_string(),
            }
        };
        let status = self
            .process
            .try_wait()?
            .map(|status| status.to_string())
            .unwrap_or_else(|| "still running".to_string());
        let mut stderr = vec![0; 8 * 1024];
        let stderr_len = timeout(Duration::from_millis(100), self.stderr.read(&mut stderr))
            .await
            .ok()
            .transpose()?
            .unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr[..stderr_len]);
        anyhow::bail!(
            "app-server closed the Unix connection ({close_reason}; status: {status}; stderr: {stderr})"
        );
    }
}

impl Drop for RelayLegacyFixture {
    fn drop(&mut self) {
        let _ = self.process.start_kill();
    }
}

async fn connect(process: &mut Child, socket_path: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => {
                if let Some(status) = process.try_wait()? {
                    anyhow::bail!("app-server exited before accepting Unix connections: {status}");
                }
                sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error).context("connect to app-server Unix socket"),
        }
    }
}
