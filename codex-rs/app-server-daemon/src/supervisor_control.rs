use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use codex_app_server_transport::AppServerInstanceIdentity;
use codex_app_server_transport::SupervisedAppServerStatus;
use codex_app_server_transport::SupervisorControlError;
use codex_app_server_transport::SupervisorControlErrorCode;
use codex_app_server_transport::SupervisorControlErrorResponse;
use codex_app_server_transport::SupervisorControlEvent;
use codex_app_server_transport::SupervisorControlEventMethod;
use codex_app_server_transport::SupervisorControlMessage;
use codex_app_server_transport::SupervisorControlRequest;
use codex_app_server_transport::SupervisorControlResponse;
use codex_app_server_transport::SupervisorSnapshot;
use codex_app_server_transport::prepare_control_socket_path;
use codex_uds::UnixListener;
use codex_uds::UnixStream;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::timeout;

const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 16;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) enum ControlCommand {
    Restart {
        expected: AppServerInstanceIdentity,
        reply: oneshot::Sender<Result<SupervisorSnapshot, SupervisorControlErrorCode>>,
    },
}

pub(crate) struct ControlServer {
    task: JoinHandle<()>,
}

impl ControlServer {
    pub(crate) async fn start(
        socket_path: PathBuf,
        snapshots: watch::Receiver<SupervisorSnapshot>,
        commands: mpsc::Sender<ControlCommand>,
    ) -> Result<Self> {
        let parent = socket_path
            .parent()
            .context("supervisor control socket has no parent directory")?;
        codex_uds::prepare_private_socket_directory(parent)
            .await
            .map_err(|err| {
                anyhow!(
                    "failed to prepare supervisor control root: {:?}",
                    err.kind()
                )
            })?;
        let lock_path = socket_path.with_extension("sock.lock");
        let lock_file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_path)
            .await
            .map_err(|err| anyhow!("failed to open supervisor control lock: {:?}", err.kind()))?;
        if !try_lock_file(&lock_file)? {
            bail!("another supervisor owns the control socket");
        }
        prepare_control_socket_path(&socket_path)
            .await
            .map_err(|err| {
                anyhow!(
                    "failed to prepare supervisor control socket: {:?}",
                    err.kind()
                )
            })?;
        let mut listener = UnixListener::bind(&socket_path)
            .await
            .map_err(|err| anyhow!("failed to bind supervisor control socket: {:?}", err.kind()))?;
        tokio::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|err| {
                anyhow!(
                    "failed to secure supervisor control socket: {:?}",
                    err.kind()
                )
            })?;
        let guard = SocketGuard { socket_path };
        let task = tokio::spawn(async move {
            let _lock_file = lock_file;
            let _guard = guard;
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok(stream) = accepted else {
                            return;
                        };
                        if connections.len() < MAX_CONNECTIONS {
                            let snapshots = snapshots.clone();
                            let commands = commands.clone();
                            connections.spawn(async move {
                                let _ = serve_connection(stream, snapshots, commands).await;
                            });
                        }
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { task })
    }

    pub(crate) async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

pub(crate) fn restart_error_for_snapshot(
    snapshot: &SupervisorSnapshot,
    expected: AppServerInstanceIdentity,
) -> Option<SupervisorControlErrorCode> {
    let Some(app_server) = snapshot.app_server.as_ref() else {
        return Some(SupervisorControlErrorCode::NoCurrentInstance);
    };
    if app_server.status != SupervisedAppServerStatus::Ready {
        return Some(SupervisorControlErrorCode::NotReady);
    }
    (app_server.instance != expected).then_some(SupervisorControlErrorCode::StaleInstance)
}

async fn serve_connection(
    stream: UnixStream,
    mut snapshots: watch::Receiver<SupervisorSnapshot>,
    commands: mpsc::Sender<ControlCommand>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    loop {
        tokio::select! {
            frame = read_frame(&mut reader) => {
                let Some(frame) = frame? else {
                    return Ok(());
                };
                let Frame::Payload(frame) = frame else {
                    write_error(&mut writer, None, SupervisorControlErrorCode::FrameTooLarge).await?;
                    return Ok(());
                };
                let request = match serde_json::from_slice::<SupervisorControlRequest>(&frame) {
                    Ok(request) => request,
                    Err(_) => {
                        write_error(&mut writer, None, SupervisorControlErrorCode::InvalidRequest).await?;
                        continue;
                    }
                };
                handle_request(&mut writer, request, &snapshots, &commands).await?;
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let snapshot = snapshots.borrow_and_update().clone();
                write_message(
                    &mut writer,
                    &SupervisorControlMessage::Event(SupervisorControlEvent {
                        event: SupervisorControlEventMethod::SnapshotUpdated,
                        snapshot,
                    }),
                )
                .await?;
            }
        }
    }
}

async fn handle_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    request: SupervisorControlRequest,
    snapshots: &watch::Receiver<SupervisorSnapshot>,
    commands: &mpsc::Sender<ControlCommand>,
) -> Result<()> {
    let id = request.id();
    let snapshot = match request {
        SupervisorControlRequest::SnapshotRead { .. } => snapshots.borrow().clone(),
        SupervisorControlRequest::AppServerRestart {
            expected_instance, ..
        } => {
            let rejection = restart_error_for_snapshot(&snapshots.borrow(), expected_instance);
            if let Some(code) = rejection {
                return write_error(writer, Some(id), code).await;
            }
            let (reply, response) = oneshot::channel();
            match commands.try_send(ControlCommand::Restart {
                expected: expected_instance,
                reply,
            }) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    return write_error(writer, Some(id), SupervisorControlErrorCode::Busy).await;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return write_error(writer, Some(id), SupervisorControlErrorCode::Internal)
                        .await;
                }
            }
            match timeout(COMMAND_TIMEOUT, response).await {
                Ok(Ok(Ok(snapshot))) => snapshot,
                Ok(Ok(Err(code))) => return write_error(writer, Some(id), code).await,
                Ok(Err(_)) | Err(_) => {
                    return write_error(writer, Some(id), SupervisorControlErrorCode::Internal)
                        .await;
                }
            }
        }
    };
    write_message(
        writer,
        &SupervisorControlMessage::Response(SupervisorControlResponse { id, snapshot }),
    )
    .await
}

enum Frame {
    Payload(Vec<u8>),
    TooLarge,
}

async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Frame>> {
    let mut frame = Vec::new();
    let limit = u64::try_from(MAX_FRAME_BYTES + 2)?;
    let read = reader.take(limit).read_until(b'\n', &mut frame).await?;
    if read == 0 {
        return Ok(None);
    }
    if frame.last() != Some(&b'\n') || frame.len() > MAX_FRAME_BYTES + 1 {
        return Ok(Some(Frame::TooLarge));
    }
    frame.pop();
    if frame.last() == Some(&b'\r') {
        frame.pop();
    }
    if frame.len() > MAX_FRAME_BYTES {
        return Ok(Some(Frame::TooLarge));
    }
    Ok(Some(Frame::Payload(frame)))
}

async fn write_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: Option<u64>,
    code: SupervisorControlErrorCode,
) -> Result<()> {
    let message = match code {
        SupervisorControlErrorCode::InvalidRequest => "invalid request",
        SupervisorControlErrorCode::FrameTooLarge => "frame too large",
        SupervisorControlErrorCode::NoCurrentInstance => "no current app-server instance",
        SupervisorControlErrorCode::NotReady => "app-server instance is not ready",
        SupervisorControlErrorCode::StaleInstance => "app-server instance identity is stale",
        SupervisorControlErrorCode::Busy => "supervisor is busy",
        SupervisorControlErrorCode::Internal => "supervisor operation failed",
    };
    write_message(
        writer,
        &SupervisorControlMessage::Error(SupervisorControlErrorResponse {
            id,
            error: SupervisorControlError {
                code,
                message: message.to_string(),
            },
        }),
    )
    .await
}

async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &SupervisorControlMessage,
) -> Result<()> {
    let encoded = serde_json::to_vec(message)?;
    if encoded.len() > MAX_FRAME_BYTES {
        bail!("supervisor control response exceeds its limit");
    }
    writer.write_all(&encoded).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

fn try_lock_file(file: &tokio::fs::File) -> Result<bool> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(err).context("failed to lock supervisor control socket")
}

struct SocketGuard {
    socket_path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
#[path = "supervisor_control_tests.rs"]
mod tests;
