use std::io;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_home_dir::ensure_owner_private;
#[cfg(windows)]
use codex_utils_home_dir::file_identity;
use codex_utils_home_dir::find_owner_home;
use codex_utils_home_dir::is_owner_private;
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

pub const SUPERVISOR_CONTRACT_VERSION: u32 = 1;
pub const SUPERVISED_APP_SERVER_IDENTITY_ENV_VAR: &str = "CODEX_SUPERVISED_APP_SERVER_IDENTITY";

fn default_supervisor_contract_version() -> u32 {
    SUPERVISOR_CONTRACT_VERSION
}

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

/// Returns whether the canonical app-server rendezvous is an owner-private
/// Unix socket. This is a path/peer ownership fence for readiness: a ready
/// proof alone is not sufficient if a stale regular file or another owner's
/// socket is present at the canonical path.
pub async fn app_server_socket_is_owner_private(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        let metadata = match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let Some(parent) = path.parent() else {
            return Ok(false);
        };
        let parent_metadata = tokio::fs::symlink_metadata(parent).await?;
        return Ok(metadata.file_type().is_socket()
            && metadata.uid() == parent_metadata.uid()
            && metadata.permissions().mode() & 0o777 == 0o600);
    }

    #[cfg(not(unix))]
    {
        // `uds_windows` uses a Winsock AF_UNIX namespace; depending on the
        // Windows build there may be no filesystem inode at the bound path.
        // The private parent directory is consequently the trust boundary.
        // If a filesystem rendezvous entry is present, validate it as well.
        match tokio::fs::symlink_metadata(path).await {
            // A pathname-backed Windows AF_UNIX endpoint does not create a
            // regular filesystem entry.  Any entry found here is therefore a
            // stale/foreign rendezvous artifact and must not be trusted,
            // even when its ACL happens to be owner-only.
            Ok(_metadata) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(parent) = path.parent() else {
                    return Ok(false);
                };
                let parent_metadata = match tokio::fs::symlink_metadata(parent).await {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error),
                };
                if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
                    return Ok(false);
                }
                Ok(is_owner_private(parent)?)
            }
            Err(error) => Err(error),
        }
    }
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
    #[serde(default = "default_supervisor_contract_version")]
    pub contract_version: u32,
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
    if let Some(parent) = path.as_path().parent() {
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(windows)]
        ensure_owner_private(parent)?;
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path).await?;
    #[cfg(windows)]
    ensure_owner_private(&temp_path)?;
    file.write_all(&serde_json::to_vec(&identity)?).await?;
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&temp_path, path.as_path()).await?;
    #[cfg(windows)]
    ensure_owner_private(path.as_path())?;
    Ok(SupervisedReadyFileGuard { path })
}

async fn ready_proof_matches_at(
    path: &Path,
    expected: AppServerInstanceIdentity,
) -> io::Result<bool> {
    #[cfg(windows)]
    {
        let metadata = match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Ok(false);
        }
        match is_owner_private(path) {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
        let before_identity = file_identity(path)?;
        let contents = match tokio::fs::read(path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let after_identity = file_identity(path)?;
        if before_identity != after_identity {
            return Ok(false);
        }
        if !is_owner_private(path)? {
            return Ok(false);
        }
        Ok(serde_json::from_slice(&contents).ok() == Some(expected))
    }
    #[cfg(not(windows))]
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
