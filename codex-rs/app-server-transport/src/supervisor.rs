use std::io;
use std::path::Path;

use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_home_dir::find_owner_home;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

const OWNER_CODEX_DIR_NAME: &str = ".codex";
const CONTROL_ROOT_DIR_NAME: &str = "app-server-control";
const APP_SERVER_SOCKET_FILE_NAME: &str = "app-server-control.sock";
const APP_SERVER_STARTUP_LOCK_FILE_NAME: &str = "app-server-startup.lock";
const SUPERVISOR_SOCKET_FILE_NAME: &str = "supervisor.sock";
const SUPERVISOR_SNAPSHOT_FILE_NAME: &str = "supervisor-snapshot.json";
const SUPERVISED_APP_SERVER_READY_FILE_NAME: &str = "app-server-ready.json";

pub const SUPERVISED_APP_SERVER_IDENTITY_ENV_VAR: &str = "CODEX_SUPERVISED_APP_SERVER_IDENTITY";

/// Fixed owner-local paths shared by the supervisor and all numbered Codex
/// account processes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalControlPaths {
    root: AbsolutePathBuf,
    app_server_socket: AbsolutePathBuf,
    app_server_startup_lock: AbsolutePathBuf,
    supervisor_socket: AbsolutePathBuf,
    supervisor_snapshot: AbsolutePathBuf,
    supervised_app_server_ready: AbsolutePathBuf,
}

impl CanonicalControlPaths {
    pub(crate) fn from_owner_home(owner_home: &Path) -> io::Result<Self> {
        let root = AbsolutePathBuf::from_absolute_path(
            owner_home
                .join(OWNER_CODEX_DIR_NAME)
                .join(CONTROL_ROOT_DIR_NAME),
        )?;
        let app_server_socket = child_path(&root, APP_SERVER_SOCKET_FILE_NAME)?;
        let app_server_startup_lock = child_path(&root, APP_SERVER_STARTUP_LOCK_FILE_NAME)?;
        let supervisor_socket = child_path(&root, SUPERVISOR_SOCKET_FILE_NAME)?;
        let supervisor_snapshot = child_path(&root, SUPERVISOR_SNAPSHOT_FILE_NAME)?;
        let supervised_app_server_ready = child_path(&root, SUPERVISED_APP_SERVER_READY_FILE_NAME)?;
        Ok(Self {
            root,
            app_server_socket,
            app_server_startup_lock,
            supervisor_socket,
            supervisor_snapshot,
            supervised_app_server_ready,
        })
    }

    pub fn root(&self) -> &AbsolutePathBuf {
        &self.root
    }

    pub fn app_server_socket(&self) -> &AbsolutePathBuf {
        &self.app_server_socket
    }

    pub fn app_server_startup_lock(&self) -> &AbsolutePathBuf {
        &self.app_server_startup_lock
    }

    pub fn supervisor_socket(&self) -> &AbsolutePathBuf {
        &self.supervisor_socket
    }

    pub fn supervisor_snapshot(&self) -> &AbsolutePathBuf {
        &self.supervisor_snapshot
    }
}

fn child_path(root: &AbsolutePathBuf, file_name: &str) -> io::Result<AbsolutePathBuf> {
    AbsolutePathBuf::from_absolute_path(root.as_path().join(file_name))
}

pub fn canonical_control_paths() -> io::Result<CanonicalControlPaths> {
    let owner_home = find_owner_home()?;
    CanonicalControlPaths::from_owner_home(owner_home.as_path())
}

pub fn canonical_app_server_control_socket_path() -> io::Result<AbsolutePathBuf> {
    Ok(canonical_control_paths()?.app_server_socket)
}

pub fn canonical_app_server_startup_lock_path() -> io::Result<AbsolutePathBuf> {
    Ok(canonical_control_paths()?.app_server_startup_lock)
}

pub fn canonical_supervisor_control_socket_path() -> io::Result<AbsolutePathBuf> {
    Ok(canonical_control_paths()?.supervisor_socket)
}

pub fn canonical_supervisor_snapshot_path() -> io::Result<AbsolutePathBuf> {
    Ok(canonical_control_paths()?.supervisor_snapshot)
}

fn canonical_supervised_app_server_ready_path() -> io::Result<AbsolutePathBuf> {
    Ok(canonical_control_paths()?.supervised_app_server_ready)
}

pub async fn invalidate_supervised_app_server_ready_proof() -> io::Result<()> {
    remove_ready_proof(canonical_supervised_app_server_ready_path()?.as_path()).await
}

pub async fn supervised_app_server_ready_proof_matches(
    expected: AppServerInstanceIdentity,
) -> io::Result<bool> {
    ready_proof_matches_at(
        canonical_supervised_app_server_ready_path()?.as_path(),
        expected,
    )
    .await
}

/// Stable identity for an initialized app-server instance. This identity is
/// deliberately separate from both snapshot and process generations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppServerInstanceIdentity {
    pub instance_id: Uuid,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SupervisedAppServerStatus {
    Starting,
    Ready,
    Backoff,
}

/// Sanitized process projection published by the owner-local supervisor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisedAppServerSnapshot {
    pub process_generation: u64,
    pub instance: AppServerInstanceIdentity,
    pub predecessor: Option<AppServerInstanceIdentity>,
    pub status: SupervisedAppServerStatus,
}

/// Monotonic supervisor projection consumed by local clients. Paths, PIDs and
/// credential material are intentionally excluded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorSnapshot {
    pub snapshot_revision: u64,
    pub app_server: Option<SupervisedAppServerSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "method")]
pub enum SupervisorControlRequest {
    #[serde(rename = "snapshot/read")]
    SnapshotRead { id: u64 },
    #[serde(rename = "appServer/restart")]
    AppServerRestart {
        id: u64,
        #[serde(rename = "expectedInstance")]
        expected_instance: AppServerInstanceIdentity,
    },
}

impl SupervisorControlRequest {
    pub fn id(&self) -> u64 {
        match self {
            Self::SnapshotRead { id } | Self::AppServerRestart { id, .. } => *id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorControlResponse {
    pub id: u64,
    pub snapshot: SupervisorSnapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SupervisorControlErrorCode {
    InvalidRequest,
    FrameTooLarge,
    NoCurrentInstance,
    NotReady,
    StaleInstance,
    Busy,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorControlError {
    pub code: SupervisorControlErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorControlErrorResponse {
    pub id: Option<u64>,
    pub error: SupervisorControlError,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SupervisorControlEventMethod {
    #[serde(rename = "snapshot/updated")]
    SnapshotUpdated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorControlEvent {
    pub event: SupervisorControlEventMethod,
    pub snapshot: SupervisorSnapshot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SupervisorControlMessage {
    Response(SupervisorControlResponse),
    Error(SupervisorControlErrorResponse),
    Event(SupervisorControlEvent),
}

pub(crate) async fn publish_supervised_ready_identity_from_env()
-> io::Result<Option<SupervisedReadyFileGuard>> {
    let Some(encoded) = std::env::var_os(SUPERVISED_APP_SERVER_IDENTITY_ENV_VAR) else {
        return Ok(None);
    };
    let encoded = encoded.into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "supervised app-server identity is not valid UTF-8",
        )
    })?;
    let identity = serde_json::from_str(&encoded).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "supervised app-server identity is malformed",
        )
    })?;
    publish_supervised_ready_identity(identity).await.map(Some)
}

async fn publish_supervised_ready_identity(
    identity: AppServerInstanceIdentity,
) -> io::Result<SupervisedReadyFileGuard> {
    write_supervised_ready_identity(canonical_supervised_app_server_ready_path()?, identity).await
}

async fn write_supervised_ready_identity(
    path: AbsolutePathBuf,
    identity: AppServerInstanceIdentity,
) -> io::Result<SupervisedReadyFileGuard> {
    use tokio::io::AsyncWriteExt;

    let temp_path = path.as_path().with_extension("json.tmp");
    let _ = tokio::fs::remove_file(&temp_path).await;
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path).await?;
    file.write_all(&serde_json::to_vec(&identity)?).await?;
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&temp_path, path.as_path()).await?;
    Ok(SupervisedReadyFileGuard { path })
}

async fn ready_proof_matches_at(
    path: &Path,
    expected: AppServerInstanceIdentity,
) -> io::Result<bool> {
    match tokio::fs::read(path).await {
        Ok(contents) => Ok(serde_json::from_slice(&contents).ok() == Some(expected)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

async fn remove_ready_proof(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub(crate) struct SupervisedReadyFileGuard {
    path: AbsolutePathBuf,
}

impl Drop for SupervisedReadyFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path.as_path());
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
