use std::io::ErrorKind;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use codex_app_server_transport::AppServerInstanceIdentity;
use codex_app_server_transport::ManagedAccountCatalog;
use codex_app_server_transport::ManagedAccountId;
use codex_app_server_transport::REMOTE_CONTROL_DISABLED_ENV_VAR;
use codex_app_server_transport::SUPERVISED_APP_SERVER_IDENTITY_ENV_VAR;
use codex_app_server_transport::SupervisedAppServerSnapshot;
use codex_app_server_transport::SupervisedAppServerStatus;
use codex_app_server_transport::SupervisorSnapshot;
use codex_app_server_transport::acquire_app_server_startup_lock;
use codex_app_server_transport::canonical_control_paths;
use codex_app_server_transport::invalidate_supervised_app_server_ready_proof;
use codex_app_server_transport::supervised_app_server_ready_proof_matches;
use codex_utils_home_dir::find_codex_home;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use uuid::Uuid;

use crate::backend::ProcessStartIdentity;
use crate::backend::process_start_identity;
use crate::backend::process_start_identity_is_active;
use crate::client;
use crate::managed_install::managed_codex_bin;
use crate::settings::DaemonSettings;
use crate::supervisor_control::ControlCommand;
use crate::supervisor_control::ControlServer;
use crate::supervisor_control::restart_error_for_snapshot;

const SETTINGS_FILE_NAME: &str = "settings.json";
const STATE_DIR_NAME: &str = "app-server-daemon";
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const STOP_GRACE: Duration = Duration::from_secs(5);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024;

struct SupervisorConfig {
    codex_bin: PathBuf,
    app_socket: PathBuf,
    control_socket: PathBuf,
    snapshot_file: PathBuf,
    remote_control_enabled: bool,
}

impl SupervisorConfig {
    async fn from_environment() -> Result<Self> {
        let codex_home = find_codex_home().context("failed to resolve CODEX_HOME")?;
        let owner_home = codex_app_server_transport::find_owner_home()
            .context("failed to resolve owner home")?;
        validate_supervisor_codex_home(codex_home.as_path(), owner_home.as_path())?;
        let codex_bin = managed_codex_bin(codex_home.as_path());
        if !codex_bin.is_file() {
            bail!("managed Codex binary is unavailable");
        }
        let paths = canonical_control_paths()?;
        let settings = DaemonSettings::load(
            &codex_home
                .as_path()
                .join(STATE_DIR_NAME)
                .join(SETTINGS_FILE_NAME),
        )
        .await?;
        Ok(Self {
            codex_bin,
            app_socket: paths.app_server_socket().as_path().to_path_buf(),
            control_socket: paths.supervisor_socket().as_path().to_path_buf(),
            snapshot_file: paths.supervisor_snapshot().as_path().to_path_buf(),
            remote_control_enabled: settings.remote_control_enabled,
        })
    }
}

/// A canonical supervisor is owner-scoped, while each child app-server keeps
/// its own numbered-home startup lock. Refuse the owner `.codex` home up
/// front: otherwise the supervisor and a child can accidentally share the
/// same lock namespace and failures are hidden behind guardian backoff.
fn validate_supervisor_codex_home(codex_home: &Path, owner_home: &Path) -> Result<()> {
    let catalog = ManagedAccountCatalog::load_from_owner_home(owner_home)
        .context("managed account catalog is invalid or unavailable")?;
    let c1 = ManagedAccountId::from_number(1).expect("account one is non-zero");
    let expected_c1 = catalog
        .home(c1)
        .ok_or_else(|| anyhow!("managed account catalog does not register C1"))?;
    let codex_home = std::fs::canonicalize(codex_home)
        .context("CODEX_HOME cannot be canonicalized for supervisor ownership")?;
    let owner_codex_home = std::fs::canonicalize(owner_home.join(".codex"))
        .context("owner .codex home cannot be canonicalized for supervisor ownership")?;
    if codex_home == owner_codex_home {
        bail!("canonical supervisor cannot run with the owner .codex CODEX_HOME");
    }
    if codex_home != expected_c1 {
        bail!(
            "canonical supervisor requires CODEX_HOME to match registered C1 ({})",
            expected_c1.display()
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Candidate {
    process_generation: u64,
    identity: AppServerInstanceIdentity,
}

struct ManagedChild {
    candidate: Candidate,
    process: Child,
    process_identity: ProcessStartIdentity,
    ready_at: Instant,
}

struct SnapshotPublisher {
    snapshot: SupervisorSnapshot,
    path: PathBuf,
    updates: watch::Sender<SupervisorSnapshot>,
}

struct SupervisorSeed {
    snapshot: SupervisorSnapshot,
    last_ready: Option<AppServerInstanceIdentity>,
    next_process_generation: u64,
    next_instance_generation: u64,
}

impl SupervisorSeed {
    fn empty() -> Self {
        Self {
            snapshot: SupervisorSnapshot {
                snapshot_revision: 0,
                app_server: None,
            },
            last_ready: None,
            next_process_generation: 0,
            next_instance_generation: 0,
        }
    }

    fn from_snapshot(snapshot: SupervisorSnapshot) -> Result<Self> {
        if snapshot.snapshot_revision == 0 {
            bail!("persisted supervisor snapshot revision must be positive");
        }
        let app_server = snapshot.app_server.as_ref().ok_or_else(|| {
            anyhow!("persisted supervisor snapshot has no generation high-water mark")
        })?;
        if app_server.process_generation == 0
            || app_server.instance.generation == 0
            || app_server.instance.instance_id.is_nil()
        {
            bail!("persisted supervisor snapshot has an invalid generation high-water mark");
        }
        if app_server.predecessor.is_some_and(|predecessor| {
            predecessor.instance_id.is_nil()
                || predecessor.generation == 0
                || predecessor.generation >= app_server.instance.generation
                || predecessor == app_server.instance
        }) {
            bail!("persisted supervisor snapshot has an invalid predecessor");
        }
        let last_ready = match app_server.status {
            SupervisedAppServerStatus::Ready => Some(app_server.instance),
            SupervisedAppServerStatus::Starting | SupervisedAppServerStatus::Backoff => {
                app_server.predecessor
            }
        };
        Ok(Self {
            next_process_generation: app_server.process_generation,
            next_instance_generation: app_server.instance.generation,
            snapshot,
            last_ready,
        })
    }
}

impl SnapshotPublisher {
    async fn publish(&mut self, app_server: Option<SupervisedAppServerSnapshot>) -> Result<()> {
        self.snapshot.snapshot_revision = self
            .snapshot
            .snapshot_revision
            .checked_add(1)
            .ok_or_else(|| anyhow!("supervisor snapshot revision exhausted"))?;
        self.snapshot.app_server = app_server;
        write_private_json(&self.path, &self.snapshot).await?;
        self.updates.send_replace(self.snapshot.clone());
        Ok(())
    }

    fn snapshot(&self) -> SupervisorSnapshot {
        self.snapshot.clone()
    }
}

struct Guardian {
    config: SupervisorConfig,
    publisher: SnapshotPublisher,
    current: Option<ManagedChild>,
    last_ready: Option<AppServerInstanceIdentity>,
    next_process_generation: u64,
    next_instance_generation: u64,
    failures: u32,
    backoff_pending: bool,
    control_commands: mpsc::Receiver<ControlCommand>,
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

impl Guardian {
    async fn run(mut self) -> Result<()> {
        loop {
            if self.current.is_none() {
                if self.backoff_pending {
                    self.backoff_pending = false;
                    if !self.wait_backoff().await? {
                        return self.shutdown().await;
                    }
                }
                match self.launch().await {
                    Ok(child) => {
                        self.current = Some(child);
                    }
                    Err(_) => {
                        self.failures = self.failures.saturating_add(1);
                        self.backoff_pending = true;
                        continue;
                    }
                }
            }

            let current = self
                .current
                .as_mut()
                .ok_or_else(|| anyhow!("supervisor lost its current child"))?;
            tokio::select! {
                status = current.process.wait() => {
                    status.context("failed waiting for supervised app-server")?;
                    let child = self
                        .current
                        .take()
                        .ok_or_else(|| anyhow!("supervisor lost its exited child"))?;
                    if child.ready_at.elapsed() >= BACKOFF_RESET_AFTER {
                        self.failures = 0;
                    }
                    let candidate = child.candidate;
                    self.publish_status(candidate, SupervisedAppServerStatus::Backoff).await?;
                    self.failures = self.failures.saturating_add(1);
                    self.backoff_pending = true;
                }
                Some(command) = self.control_commands.recv() => self.handle_control(command).await,
                _ = self.terminate.recv() => return self.shutdown().await,
                _ = self.interrupt.recv() => return self.shutdown().await,
            }
        }
    }

    async fn launch(&mut self) -> Result<ManagedChild> {
        let candidate = self.next_candidate()?;
        self.publish_status(candidate, SupervisedAppServerStatus::Starting)
            .await?;
        invalidate_supervised_app_server_ready_proof().await?;
        let mut command = Command::new(&self.config.codex_bin);
        command
            .arg("app-server")
            .arg("--listen")
            .arg(format!("unix://{}", self.config.app_socket.display()));
        if self.config.remote_control_enabled {
            command.arg("--remote-control");
            command.env_remove(REMOTE_CONTROL_DISABLED_ENV_VAR);
        } else {
            command.env(REMOTE_CONTROL_DISABLED_ENV_VAR, "1");
        }
        command
            .env(
                SUPERVISED_APP_SERVER_IDENTITY_ENV_VAR,
                serde_json::to_string(&candidate.identity)?,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut process = match command
            .spawn()
            .context("failed to spawn supervised app-server")
        {
            Ok(process) => process,
            Err(err) => return self.launch_failed(candidate, err).await,
        };
        let Some(pid) = process.id() else {
            return self
                .launch_failed(candidate, anyhow!("supervised app-server has no pid"))
                .await;
        };
        let process_identity = match process_start_identity(pid).await {
            Ok(identity) => identity,
            Err(err) => return self.launch_failed(candidate, err).await,
        };
        let ready = self
            .wait_ready(&mut process, &process_identity, candidate.identity)
            .await;
        if let Err(err) = ready {
            let mut child = ManagedChild {
                candidate,
                process,
                process_identity,
                ready_at: Instant::now(),
            };
            let _ = stop_exact(&mut child).await;
            self.publish_status(candidate, SupervisedAppServerStatus::Backoff)
                .await?;
            return Err(err);
        }
        self.last_ready = Some(candidate.identity);
        self.publisher
            .publish(Some(SupervisedAppServerSnapshot {
                process_generation: candidate.process_generation,
                instance: candidate.identity,
                predecessor: None,
                status: SupervisedAppServerStatus::Ready,
            }))
            .await?;
        Ok(ManagedChild {
            candidate,
            process,
            process_identity,
            ready_at: Instant::now(),
        })
    }

    async fn wait_ready(
        &self,
        process: &mut Child,
        process_identity: &ProcessStartIdentity,
        expected: AppServerInstanceIdentity,
    ) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = process.try_wait()? {
                bail!("supervised app-server exited before readiness: {status}");
            }
            if !process_start_identity_is_active(process_identity).await? {
                bail!("supervised app-server start identity is no longer active");
            }
            if supervised_app_server_ready_proof_matches(expected).await?
                && codex_app_server_transport::app_server_socket_is_owner_private(
                    &self.config.app_socket,
                )
                .await?
                && client::probe(&self.config.app_socket).await.is_ok()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("supervised app-server readiness timed out");
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    fn next_candidate(&mut self) -> Result<Candidate> {
        self.next_process_generation = self
            .next_process_generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("process generation exhausted"))?;
        self.next_instance_generation = self
            .next_instance_generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("app-server instance generation exhausted"))?;
        Ok(Candidate {
            process_generation: self.next_process_generation,
            identity: AppServerInstanceIdentity {
                instance_id: Uuid::new_v4(),
                generation: self.next_instance_generation,
            },
        })
    }

    async fn publish_status(
        &mut self,
        candidate: Candidate,
        status: SupervisedAppServerStatus,
    ) -> Result<()> {
        let predecessor = (self.last_ready != Some(candidate.identity))
            .then_some(self.last_ready)
            .flatten();
        self.publisher
            .publish(Some(SupervisedAppServerSnapshot {
                process_generation: candidate.process_generation,
                instance: candidate.identity,
                predecessor,
                status,
            }))
            .await
    }

    async fn launch_failed<T>(&mut self, candidate: Candidate, err: anyhow::Error) -> Result<T> {
        self.publish_status(candidate, SupervisedAppServerStatus::Backoff)
            .await?;
        Err(err)
    }

    async fn wait_backoff(&mut self) -> Result<bool> {
        let delay = backoff_delay(self.failures);
        tokio::select! {
            _ = sleep(delay) => Ok(true),
            _ = self.terminate.recv() => Ok(false),
            _ = self.interrupt.recv() => Ok(false),
        }
    }

    async fn handle_control(&mut self, command: ControlCommand) {
        match command {
            ControlCommand::Restart { expected, reply } => {
                let result = self.restart_exact(expected).await;
                let _ = reply.send(result);
            }
        }
    }

    async fn restart_exact(
        &mut self,
        expected: AppServerInstanceIdentity,
    ) -> Result<SupervisorSnapshot, codex_app_server_transport::SupervisorControlErrorCode> {
        if let Some(code) = restart_error_for_snapshot(&self.publisher.snapshot, expected) {
            return Err(code);
        }
        let Some(mut child) = self.current.take() else {
            return Err(codex_app_server_transport::SupervisorControlErrorCode::NoCurrentInstance);
        };
        if child.candidate.identity != expected {
            self.current = Some(child);
            return Err(codex_app_server_transport::SupervisorControlErrorCode::StaleInstance);
        }
        if stop_exact(&mut child).await.is_err() {
            self.current = Some(child);
            return Err(codex_app_server_transport::SupervisorControlErrorCode::Internal);
        }
        match self.launch().await {
            Ok(child) => {
                self.current = Some(child);
                Ok(self.publisher.snapshot())
            }
            Err(_) => {
                self.failures = self.failures.saturating_add(1);
                self.backoff_pending = true;
                Err(codex_app_server_transport::SupervisorControlErrorCode::Internal)
            }
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(mut child) = self.current.take() {
            stop_exact(&mut child).await?;
        }
        invalidate_supervised_app_server_ready_proof().await?;
        let Some(mut app_server) = self.publisher.snapshot.app_server.clone() else {
            return Ok(());
        };
        app_server.status = SupervisedAppServerStatus::Backoff;
        self.publisher.publish(Some(app_server)).await
    }
}

pub(crate) async fn run() -> Result<()> {
    let config = SupervisorConfig::from_environment().await?;
    let startup_lock = canonical_control_paths()?.app_server_startup_lock().clone();
    let _ownership = acquire_app_server_startup_lock(startup_lock).await?;
    if client::probe(&config.app_socket).await.is_ok() {
        bail!("canonical app-server is already owned outside this supervisor");
    }
    let seed = load_supervisor_seed(&config.snapshot_file).await?;
    let initial = seed.snapshot;
    let (updates, snapshots) = watch::channel(initial.clone());
    let publisher = SnapshotPublisher {
        snapshot: initial,
        path: config.snapshot_file.clone(),
        updates,
    };
    let (control_commands, commands) = mpsc::channel(8);
    let control_server =
        ControlServer::start(config.control_socket.clone(), snapshots, control_commands).await?;
    let guardian = Guardian {
        config,
        publisher,
        current: None,
        last_ready: seed.last_ready,
        next_process_generation: seed.next_process_generation,
        next_instance_generation: seed.next_instance_generation,
        failures: 0,
        backoff_pending: false,
        control_commands: commands,
        terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
    };
    let result = guardian.run().await;
    control_server.shutdown().await;
    result
}

async fn load_supervisor_seed(path: &Path) -> Result<SupervisorSeed> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_supervisor_seed(&path))
        .await
        .map_err(|err| anyhow!("supervisor snapshot read task failed: {err}"))?
}

fn read_supervisor_seed(path: &Path) -> Result<SupervisorSeed> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(SupervisorSeed::empty()),
        Err(err) => return Err(err.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.len() > MAX_SNAPSHOT_BYTES
    {
        bail!("persisted supervisor snapshot is not a private owner file");
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_SNAPSHOT_BYTES {
        bail!("persisted supervisor snapshot is too large");
    }
    let snapshot =
        serde_json::from_slice(&contents).context("persisted supervisor snapshot is malformed")?;
    SupervisorSeed::from_snapshot(snapshot)
}

async fn write_private_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    if let Some(parent) = path.parent() {
        codex_uds::prepare_private_socket_directory(parent).await?;
    }
    let temp = path.with_extension("json.tmp");
    remove_if_exists(&temp).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .await?;
    file.write_all(&serde_json::to_vec(value)?).await?;
    file.flush().await?;
    drop(file);
    tokio::fs::rename(temp, path).await?;
    Ok(())
}

async fn remove_if_exists(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

async fn stop_exact(child: &mut ManagedChild) -> Result<()> {
    if child.process.try_wait()?.is_some() {
        return Ok(());
    }
    if !process_start_identity_is_active(&child.process_identity).await? {
        return Ok(());
    }
    let pid = libc::pid_t::try_from(child.process_identity.pid())?;
    if unsafe { libc::kill(pid, libc::SIGTERM) } == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err.into());
        }
    }
    match timeout(STOP_GRACE, child.process.wait()).await {
        Ok(status) => {
            status?;
        }
        Err(_) if process_start_identity_is_active(&child.process_identity).await? => {
            child.process.kill().await?;
        }
        Err(_) => {}
    }
    Ok(())
}

fn backoff_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    BACKOFF_BASE
        .saturating_mul(1_u32 << exponent)
        .min(BACKOFF_MAX)
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
