//! Opt-in actual Windows CLI -> test TCP gate -> existing SSH forward -> Mac runtime.
//! The caller owns SSH and a fresh `remote-smoke-authority.py --hold-first-response` fixture.
//! No SSH, remote launch, provider credentials, or package compilation belongs to this test.

use super::*;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::RequestId;
use serde::Deserialize;
use serde_json::Value;
use std::net::SocketAddr;
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use url::Url;

const MODEL: &str = "remote-smoke-model";
const PROVIDER: &str = "remote-smoke";
const DRAFT: &str = "unsent-real-endpoint-offline-draft";

#[derive(Deserialize)]
struct FixtureStatus {
    observed: bool,
    released: bool,
    response_count: usize,
    completed_response_count: usize,
    phases: Vec<String>,
    turn_ids: Vec<Option<String>>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Mac fixture and caller-owned SSH forwards; never launches SSH"]
async fn windows_real_remote_goal_continues_offline_and_rejoins_with_draft() -> Result<()> {
    let remote = loopback_url("CODEX_REAL_REMOTE_WS", "ws")?;
    let fixture = loopback_url("CODEX_REAL_REMOTE_FIXTURE_HTTP", "http")?;
    let cwd = std::env::var("CODEX_REAL_REMOTE_CWD").context("missing CODEX_REAL_REMOTE_CWD")?;
    ensure!(
        cwd.starts_with('/') && !cwd.chars().any(char::is_control),
        "expected an absolute Mac cwd"
    );
    ensure!(
        std::env::var_os("CARGO_BIN_EXE_codex").is_some(),
        "select a prebuilt CLI with CARGO_BIN_EXE_codex"
    );
    let http = codex_http_client::HttpClientBuilder::new()
        .without_redirects()
        .build_direct()?;
    let status_url = fixture.join("/fixture/first-request")?;
    let initial = fixture_status(&http, &status_url).await?;
    ensure!(
        !initial.observed
            && !initial.released
            && initial.response_count == 0
            && initial.completed_response_count == 0,
        "fixture must be fresh with its first response held"
    );
    let observer = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: remote.to_string(),
            auth_token: None,
        },
        client_name: "windows-real-remote-smoke".into(),
        client_version: "0.1.0".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await
    .map_err(|_| anyhow::anyhow!("could not initialize the actual app-server observer"))?;
    let listed = rpc(
        &observer,
        "thread/list",
        json!({"limit": 100, "modelProviders": []}),
    )
    .await?;
    ensure!(
        listed["data"].as_array().is_some_and(Vec::is_empty),
        "Mac app-server owner must have no existing threads"
    );

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut endpoint = remote.clone();
    endpoint.set_host(Some("127.0.0.1"))?;
    endpoint
        .set_port(Some(listener.local_addr()?.port()))
        .map_err(|_| anyhow::anyhow!("invalid proxy port"))?;
    let target: SocketAddr = remote
        .socket_addrs(|| None)?
        .into_iter()
        .next()
        .context("missing SSH address")?;
    let (cut_tx, cut_rx) = oneshot::channel();
    let (cut_ack_tx, cut_ack_rx) = oneshot::channel();
    let (restore_tx, restore_rx) = oneshot::channel();
    let proxy = Proxy(tokio::spawn(proxy_connections(
        listener, target, cut_rx, cut_ack_tx, restore_rx,
    )));
    let (_home, mut terminal) = start_terminal(endpoint.as_str(), &cwd).await?;
    terminal.wait_for(&[MODEL, &cwd]).await?;
    terminal.write(b"/status\r").await?;
    terminal
        .wait_for(&["Model provider", PROVIDER, &cwd])
        .await?;
    let screen = terminal.parser.screen().contents();
    for forbidden in [
        "windows-only-model",
        "windows-only-provider",
        "windows-client-project",
    ] {
        ensure!(
            !screen.contains(forbidden),
            "Windows-only authority appeared in remote status"
        );
    }
    let objective = format!("Complete isolated remote smoke {}", uuid::Uuid::new_v4());
    terminal
        .write(format!("/goal {objective}\r").as_bytes())
        .await?;
    terminal.wait_for(&["Goal active"]).await?;
    wait_fixture(&http, &status_url, /*completed*/ 0).await?;
    let listed = rpc(
        &observer,
        "thread/list",
        json!({"limit": 100, "modelProviders": []}),
    )
    .await?;
    let threads = listed["data"]
        .as_array()
        .context("thread list missing data")?;
    ensure!(
        threads.len() == 1,
        "fixture must contain exactly one user thread"
    );
    let thread_id = threads[0]["id"]
        .as_str()
        .context("missing thread ID")?
        .to_string();
    ensure!(
        threads[0]["cwd"] == cwd,
        "actual thread cwd differs from the requested Mac cwd"
    );
    let active = rpc(&observer, "thread/goal/get", json!({"threadId": thread_id})).await?;
    let goal_id = active["goal"]["goalId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("active goal has no versioned identity")?
        .to_string();
    let revision = active["goal"]["revision"]
        .as_i64()
        .filter(|revision| *revision > 0)
        .context("active goal has no positive revision")?;
    ensure!(
        active["goal"]["status"] == "active" && active["goal"]["objective"] == objective,
        "CLI did not create the expected active goal"
    );

    cut_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("proxy exited before requested disconnect"))?;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), cut_ack_rx)
        .await
        .context("proxy disconnect acknowledgment timed out")?
        .context("proxy did not acknowledge closing both first-connection sockets")?;
    // Accepted recovery handshakes stay queued behind the gate; no upstream retry policy changes.
    // This bounds all offline checks below the existing five-attempt/120s recovery budget.
    let offline = async {
        terminal.wait_for(&["Reconnecting"]).await?;
        terminal.write(DRAFT.as_bytes()).await?;
        terminal.wait_for(&[DRAFT]).await?;
        let response = http
            .post(fixture.join("/fixture/release-first-response")?)
            .timeout(Duration::from_secs(/*secs*/ 5))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("fixture release request failed"))?;
        ensure!(
            response.status().is_success(),
            "fixture refused the first-response release"
        );
        let completed = wait_fixture(&http, &status_url, /*completed*/ 5).await?;
        validate_goal_exchanges(
            completed.response_count,
            completed.completed_response_count,
            &completed.phases,
            &completed.turn_ids,
        )?;
        let goal = rpc(&observer, "thread/goal/get", json!({"threadId": thread_id})).await?;
        ensure!(
            goal["goal"]["status"] == "complete"
                && goal["goal"]["objective"] == objective
                && goal["goal"]["goalId"] == goal_id
                && goal["goal"]["revision"]
                    .as_i64()
                    .is_some_and(|value| value > revision),
            "actual goal did not complete with the original identity and a newer revision"
        );
        let deadline = Instant::now() + Duration::from_secs(/*secs*/ 5);
        loop {
            let history = rpc(
                &observer,
                "thread/turns/list",
                json!({
                    "threadId": thread_id, "limit": 10, "itemsView": "full"
                }),
            )
            .await?;
            let turns = history["data"]
                .as_array()
                .context("missing actual turn history")?;
            ensure!(turns.len() <= 2, "unexpected extra backend turn");
            if turns.len() == 2 && turns.iter().all(|turn| turn["status"] == "completed") {
                ensure!(
                    turns
                        .iter()
                        .filter_map(|turn| turn["items"].as_array())
                        .flatten()
                        .filter(|item| item["type"] == "commandExecution")
                        .count()
                        == 1,
                    "goal resync must not replay pwd"
                );
                ensure!(
                    turns
                        .iter()
                        .filter_map(|turn| turn["items"].as_array())
                        .flatten()
                        .any(|item| {
                            item["type"] == "commandExecution"
                                && item["cwd"] == cwd
                                && item["exitCode"] == 0
                                && item["aggregatedOutput"].as_str().is_some_and(|output| {
                                    output.lines().any(|line| line.trim() == cwd)
                                })
                        }),
                    "actual pwd output did not confirm the Mac execution cwd"
                );
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "backend turns did not finish before reconnect"
            );
            tokio::time::sleep(Duration::from_millis(/*millis*/ 25)).await;
        }
        Ok::<_, anyhow::Error>(completed)
    };
    let completed = tokio::time::timeout(Duration::from_secs(/*secs*/ 40), offline)
        .await
        .context("offline continuation exceeded the recovery window")??;
    restore_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("proxy exited before recovery"))?;
    terminal
        .wait_for(&["Reconnected.", DRAFT, "remote smoke goal complete"])
        .await?;

    // Configured Alt+A dispatches the same OpenAgentsOverview event as /agents, without erasing input.
    terminal.write(b"\x1ba").await?;
    terminal.wait_for(&["Agent command center", &cwd]).await?;
    terminal.write(b"\r").await?;
    terminal
        .wait_for_screen(&[DRAFT, MODEL], &["Agent command center"])
        .await?;
    // The draft assertion is complete. Explicitly erase it; never submit it to the model.
    terminal.write(b"\x15/agents\r").await?;
    terminal.wait_for(&["Agent command center", &cwd]).await?;
    terminal.write(b"\r").await?;
    terminal
        .wait_for_screen(&[MODEL], &["Agent command center"])
        .await?;
    // Resuming the active ID is intentionally a no-op. /clear starts an empty thread;
    // returning from it exercises real resume/history hydration without a model turn.
    terminal.write(b"/clear\r").await?;
    terminal
        .wait_for_screen(&[MODEL, &cwd], &["remote smoke goal complete"])
        .await?;
    terminal
        .write(format!("/resume {thread_id}\r").as_bytes())
        .await?;
    terminal
        .wait_for(&[MODEL, "remote smoke goal complete"])
        .await?;
    terminal.write(b"/goal\r").await?;
    terminal.wait_for(&["Status: complete", &objective]).await?;
    let final_status = fixture_status(&http, &status_url).await?;
    ensure!(
        final_status.response_count == completed.response_count
            && final_status.completed_response_count == completed.completed_response_count
            && final_status.phases == completed.phases
            && final_status.turn_ids == completed.turn_ids,
        "navigation or recovery issued an unexpected model request"
    );
    ensure!(!proxy.0.is_finished(), "TCP proxy exited unexpectedly");
    drop(terminal);
    observer
        .shutdown()
        .await
        .map_err(|_| anyhow::anyhow!("observer shutdown failed"))?;
    Ok(())
}

pub(super) fn loopback_url(name: &str, scheme: &str) -> Result<Url> {
    let value = std::env::var(name).with_context(|| format!("missing {name}"))?;
    let url = Url::parse(&value).map_err(|_| anyhow::anyhow!("invalid {name}"))?;
    ensure!(
        url.scheme() == scheme
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && match url.host() {
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                _ => false,
            }
            && url.port().is_some(),
        "{name} must be a credential-free numeric loopback URL with an explicit port"
    );
    Ok(url)
}

async fn fixture_status(http: &codex_http_client::HttpClient, url: &Url) -> Result<FixtureStatus> {
    let response = http
        .get(url.clone())
        .timeout(Duration::from_secs(/*secs*/ 5))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("fixture status request failed"))?;
    ensure!(
        response.status().is_success(),
        "fixture status HTTP failure"
    );
    response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("fixture status has an unsupported schema"))
}

async fn wait_fixture(
    http: &codex_http_client::HttpClient,
    url: &Url,
    completed: usize,
) -> Result<FixtureStatus> {
    let deadline = Instant::now() + Duration::from_secs(/*secs*/ 30);
    loop {
        let state = fixture_status(http, url).await?;
        ensure!(
            state.response_count <= 11 && state.completed_response_count <= state.response_count,
            "fixture received unexpected requests"
        );
        if completed == 0 && state.observed {
            ensure!(
                !state.released && state.response_count == 1 && state.completed_response_count == 0,
                "first response was not held until disconnect"
            );
            return Ok(state);
        }
        if completed == 5
            && state
                .phases
                .last()
                .is_some_and(|phase| phase == "goal_complete")
            && state.completed_response_count == state.response_count
        {
            ensure!(state.released, "completed fixture was never released");
            return Ok(state);
        }
        ensure!(
            Instant::now() < deadline,
            "fixture did not reach the required phase before its deadline"
        );
        tokio::time::sleep(Duration::from_millis(/*millis*/ 25)).await;
    }
}

// The fixture permits at most three CAS resyncs, never another command or turn.
pub(super) fn validate_goal_exchanges(
    response_count: usize,
    completed_response_count: usize,
    phases: &[String],
    turn_ids: &[Option<String>],
) -> Result<()> {
    ensure!(
        (5..=11).contains(&response_count)
            && response_count % 2 == 1
            && completed_response_count == response_count
            && phases.len() == response_count
            && phases[..4] == ["pwd", "first_turn_complete", "get_goal", "update_goal"]
            && phases.last().is_some_and(|phase| phase == "goal_complete")
            && phases[4..response_count - 1]
                .chunks_exact(2)
                .all(|pair| { pair == ["get_goal_resync", "update_goal_retry"] }),
        "expected five goal exchanges plus bounded CAS resync pairs"
    );
    ensure!(
        turn_ids.len() == response_count
            && turn_ids
                .iter()
                .all(|id| id.as_ref().is_some_and(|id| !id.is_empty()))
            && turn_ids[0] == turn_ids[1]
            && turn_ids[2..].iter().all(|id| id == &turn_ids[2])
            && turn_ids[0] != turn_ids[2],
        "expected one initial turn and a separate automatic goal continuation"
    );
    Ok(())
}

pub(super) async fn rpc(
    client: &RemoteAppServerClient,
    method: &str,
    params: Value,
) -> Result<Value> {
    let handle = client.request_handle();
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        handle.request_json_rpc(JSONRPCRequest {
            id: RequestId::String(uuid::Uuid::new_v4().to_string()),
            method: method.to_string(),
            params: Some(params),
            trace: None,
        }),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{method} timed out"))?
    .map_err(|_| anyhow::anyhow!("{method} transport failed"))?
    .map_err(|_| anyhow::anyhow!("{method} was rejected by the actual server"))
}

struct Proxy(JoinHandle<Result<()>>);

impl Drop for Proxy {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn proxy_connections(
    listener: TcpListener,
    target: SocketAddr,
    cut: oneshot::Receiver<()>,
    cut_ack: oneshot::Sender<()>,
    restore: oneshot::Receiver<()>,
) -> Result<()> {
    {
        let (mut client, _) = listener.accept().await?;
        let mut upstream = TcpStream::connect(target).await?;
        tokio::select! {
            result = copy_bidirectional(&mut client, &mut upstream) => {
                result?;
                anyhow::bail!("initial client connection ended before the requested cut");
            }
            result = cut => { result.context("disconnect controller dropped")?; }
        }
    }
    let _ = cut_ack.send(());
    restore.await.context("reconnect controller dropped")?;
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut client, _) = accepted?;
                connections.spawn(async move {
                    if let Ok(mut upstream) = TcpStream::connect(target).await {
                        let _ = copy_bidirectional(&mut client, &mut upstream).await;
                    }
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}
