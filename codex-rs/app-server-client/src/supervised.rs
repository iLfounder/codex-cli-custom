use std::future::Future;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use codex_app_server_transport::AppServerInstanceIdentity;
use codex_app_server_transport::SupervisedAppServerStatus;
use codex_app_server_transport::SupervisorSnapshot;
use codex_app_server_transport::canonical_control_paths;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::sleep;
use tokio::time::timeout;

use crate::AppServerEvent;
use crate::RequestResult;
use crate::TypedRequestError;
use crate::remote::RemoteAppServerClient;
use crate::remote::RemoteAppServerConnectArgs;
use crate::remote::RemoteAppServerEndpoint;
use crate::remote::RemoteAppServerRequestHandle;

const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(50);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);
const SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub type SupervisorSnapshotFuture<'a> =
    Pin<Box<dyn Future<Output = IoResult<SupervisorSnapshot>> + Send + 'a>>;

/// Supplies the latest owner-local supervisor snapshot to a reconnecting client.
///
/// Implementations return one complete snapshot per read. Consumers enforce snapshot and app
/// instance monotonicity; a source must not interpret process generation as connection identity.
pub trait SupervisorSnapshotSource: Send + Sync {
    fn read_snapshot(&self) -> SupervisorSnapshotFuture<'_>;
}

struct FileSupervisorSnapshotSource {
    path: AbsolutePathBuf,
}

impl SupervisorSnapshotSource for FileSupervisorSnapshotSource {
    fn read_snapshot(&self) -> SupervisorSnapshotFuture<'_> {
        Box::pin(async move {
            let contents = tokio::fs::read(self.path.as_path()).await?;
            serde_json::from_slice(&contents).map_err(|error| {
                IoError::new(
                    ErrorKind::InvalidData,
                    format!("supervisor snapshot is invalid: {error}"),
                )
            })
        })
    }
}

#[derive(Clone, Debug)]
pub struct SupervisedAppServerConnectArgs {
    pub client_name: String,
    pub client_version: String,
    pub experimental_api: bool,
    pub mcp_server_openai_form_elicitation: bool,
    pub opt_out_notification_methods: Vec<String>,
    pub channel_capacity: usize,
}

impl SupervisedAppServerConnectArgs {
    fn remote_args(&self, socket_path: AbsolutePathBuf) -> RemoteAppServerConnectArgs {
        RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
            client_name: self.client_name.clone(),
            client_version: self.client_version.clone(),
            experimental_api: self.experimental_api,
            mcp_server_openai_form_elicitation: self.mcp_server_openai_form_elicitation,
            opt_out_notification_methods: self.opt_out_notification_methods.clone(),
            channel_capacity: self.channel_capacity,
        }
    }
}

#[derive(Debug, Clone)]
pub enum SupervisedAppServerEvent {
    Connected { identity: AppServerInstanceIdentity },
    AppServer(AppServerEvent),
}

#[derive(Clone)]
struct ActiveConnection {
    identity: AppServerInstanceIdentity,
    handle: RemoteAppServerRequestHandle,
}

type SharedConnection = Arc<RwLock<Option<ActiveConnection>>>;

pub struct SupervisedAppServerClient {
    request_handle: SupervisedAppServerRequestHandle,
    event_rx: mpsc::UnboundedReceiver<SupervisedAppServerEvent>,
    shutdown_tx: watch::Sender<bool>,
    worker_handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
pub struct SupervisedAppServerRequestHandle {
    connection: SharedConnection,
}

impl SupervisedAppServerClient {
    pub fn start(args: SupervisedAppServerConnectArgs) -> IoResult<Self> {
        let paths = canonical_control_paths()?;
        let source = Arc::new(FileSupervisorSnapshotSource {
            path: paths.supervisor_snapshot().clone(),
        });
        Ok(Self::start_at(
            args,
            paths.app_server_socket().clone(),
            source,
        ))
    }

    pub fn start_with_snapshot_source(
        args: SupervisedAppServerConnectArgs,
        source: Arc<dyn SupervisorSnapshotSource>,
    ) -> IoResult<Self> {
        let socket_path = canonical_control_paths()?.app_server_socket().clone();
        Ok(Self::start_at(args, socket_path, source))
    }

    fn start_at(
        args: SupervisedAppServerConnectArgs,
        socket_path: AbsolutePathBuf,
        source: Arc<dyn SupervisorSnapshotSource>,
    ) -> Self {
        let connection = Arc::new(RwLock::new(None));
        let request_handle = SupervisedAppServerRequestHandle {
            connection: Arc::clone(&connection),
        };
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker_handle = tokio::spawn(run_reconnect_loop(
            args,
            socket_path,
            source,
            connection,
            event_tx,
            shutdown_rx,
        ));
        Self {
            request_handle,
            event_rx,
            shutdown_tx,
            worker_handle,
        }
    }

    pub fn request_handle(&self) -> SupervisedAppServerRequestHandle {
        self.request_handle.clone()
    }

    pub fn connected_identity(&self) -> Option<AppServerInstanceIdentity> {
        self.request_handle.connected_identity()
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        self.request_handle.request(request).await
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        self.request_handle.request_typed(request).await
    }

    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        self.request_handle.notify(notification).await
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        self.request_handle
            .resolve_server_request(request_id, result)
            .await
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        self.request_handle
            .reject_server_request(request_id, error)
            .await
    }

    pub async fn next_event(&mut self) -> Option<SupervisedAppServerEvent> {
        self.event_rx.recv().await
    }

    pub async fn shutdown(self) {
        let Self {
            request_handle: _,
            event_rx,
            shutdown_tx,
            worker_handle,
        } = self;
        drop(event_rx);
        let _ = shutdown_tx.send(true);
        let mut worker_handle = worker_handle;
        if timeout(SHUTDOWN_TIMEOUT, &mut worker_handle).await.is_err() {
            worker_handle.abort();
            let _ = worker_handle.await;
        }
    }
}

impl SupervisedAppServerRequestHandle {
    pub fn connected_identity(&self) -> Option<AppServerInstanceIdentity> {
        self.connection
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|active| active.identity)
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        self.active_handle()?.request(request).await
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request.method_name();
        let response =
            self.request(request)
                .await
                .map_err(|source| TypedRequestError::Transport {
                    method: method.to_string(),
                    source,
                })?;
        let result = response.map_err(|source| TypedRequestError::Server {
            method: method.to_string(),
            source,
        })?;
        serde_json::from_value(result).map_err(|source| TypedRequestError::Deserialize {
            method: method.to_string(),
            source,
        })
    }

    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        self.active_handle()?.notify(notification).await
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        self.active_handle()?
            .resolve_server_request(request_id, result)
            .await
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        self.active_handle()?
            .reject_server_request(request_id, error)
            .await
    }

    fn active_handle(&self) -> IoResult<RemoteAppServerRequestHandle> {
        self.connection
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|active| active.handle.clone())
            .ok_or_else(unavailable)
    }
}

async fn run_reconnect_loop(
    args: SupervisedAppServerConnectArgs,
    socket_path: AbsolutePathBuf,
    source: Arc<dyn SupervisorSnapshotSource>,
    connection: SharedConnection,
    event_tx: mpsc::UnboundedSender<SupervisedAppServerEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut last_snapshot_revision = 0;
    let mut last_identity: Option<AppServerInstanceIdentity> = None;
    let mut backoff = RECONNECT_BACKOFF_BASE;
    loop {
        if shutdown_requested(&shutdown_rx) {
            break;
        }
        let snapshot = match source.read_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(_) => {
                if wait_or_shutdown(backoff, &mut shutdown_rx).await {
                    break;
                }
                backoff = next_backoff(backoff);
                continue;
            }
        };
        if snapshot.snapshot_revision < last_snapshot_revision {
            if wait_or_shutdown(SNAPSHOT_POLL_INTERVAL, &mut shutdown_rx).await {
                break;
            }
            continue;
        }
        let Some(candidate) = snapshot.app_server.as_ref() else {
            last_snapshot_revision = snapshot.snapshot_revision;
            if wait_or_shutdown(SNAPSHOT_POLL_INTERVAL, &mut shutdown_rx).await {
                break;
            }
            continue;
        };
        let same_ready_identity = last_identity == Some(candidate.instance);
        let advances_identity = last_identity
            .is_none_or(|identity| candidate.instance.generation > identity.generation);
        if candidate.status != SupervisedAppServerStatus::Ready
            || (!same_ready_identity && !advances_identity)
            || (snapshot.snapshot_revision == last_snapshot_revision && !same_ready_identity)
        {
            last_snapshot_revision = last_snapshot_revision.max(snapshot.snapshot_revision);
            if wait_or_shutdown(SNAPSHOT_POLL_INTERVAL, &mut shutdown_rx).await {
                break;
            }
            continue;
        }

        let mut client =
            match RemoteAppServerClient::connect(args.remote_args(socket_path.clone())).await {
                Ok(client) => client,
                Err(_) => {
                    if wait_or_shutdown(backoff, &mut shutdown_rx).await {
                        break;
                    }
                    backoff = next_backoff(backoff);
                    continue;
                }
            };
        let confirmed = source.read_snapshot().await;
        let confirmed_ready = confirmed.as_ref().is_ok_and(|confirmed| {
            confirmed.snapshot_revision >= snapshot.snapshot_revision
                && confirmed.app_server.as_ref().is_some_and(|app_server| {
                    app_server.status == SupervisedAppServerStatus::Ready
                        && app_server.instance == candidate.instance
                })
        });
        if !confirmed_ready {
            if let Ok(confirmed) = confirmed
                && confirmed.app_server.as_ref().is_some_and(|app_server| {
                    app_server.instance == candidate.instance
                        && app_server.status != SupervisedAppServerStatus::Ready
                })
            {
                last_snapshot_revision = confirmed.snapshot_revision;
            }
            let _ = client.shutdown().await;
            if wait_or_shutdown(backoff, &mut shutdown_rx).await {
                break;
            }
            backoff = next_backoff(backoff);
            continue;
        }
        let Ok(confirmed) = confirmed else {
            continue;
        };
        last_snapshot_revision = confirmed.snapshot_revision;
        let identity = candidate.instance;
        let remote_handle = client.request_handle();
        *connection
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ActiveConnection {
            identity,
            handle: remote_handle,
        });
        last_identity = Some(identity);
        backoff = RECONNECT_BACKOFF_BASE;
        let _ = event_tx.send(SupervisedAppServerEvent::Connected { identity });

        loop {
            tokio::select! {
                event = client.next_event() => {
                    let Some(event) = event else {
                        clear_connection(&connection, identity);
                        let _ = event_tx.send(SupervisedAppServerEvent::AppServer(
                            AppServerEvent::Disconnected {
                                message: "supervised app-server connection closed".to_string(),
                            },
                        ));
                        break;
                    };
                    let disconnected = matches!(event, AppServerEvent::Disconnected { .. });
                    if disconnected {
                        clear_connection(&connection, identity);
                    }
                    let _ = event_tx.send(SupervisedAppServerEvent::AppServer(event));
                    if disconnected {
                        break;
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || shutdown_requested(&shutdown_rx) {
                        break;
                    }
                }
            }
        }
        clear_connection(&connection, identity);
        let _ = client.shutdown().await;
        if shutdown_requested(&shutdown_rx) {
            break;
        }
    }
    *connection
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

fn clear_connection(connection: &SharedConnection, identity: AppServerInstanceIdentity) {
    let mut active = connection
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if active
        .as_ref()
        .is_some_and(|active| active.identity == identity)
    {
        *active = None;
    }
}

fn unavailable() -> IoError {
    IoError::new(
        ErrorKind::NotConnected,
        "supervised app-server is unavailable",
    )
}

fn shutdown_requested(shutdown_rx: &watch::Receiver<bool>) -> bool {
    *shutdown_rx.borrow() || shutdown_rx.has_changed().is_err()
}

async fn wait_or_shutdown(duration: Duration, shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = sleep(duration) => false,
        changed = shutdown_rx.changed() => changed.is_err() || shutdown_requested(shutdown_rx),
    }
}

fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(RECONNECT_BACKOFF_MAX)
}

#[cfg(test)]
#[path = "supervised_tests.rs"]
mod tests;
