use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_protocol::ThreadId;
use codex_state::WriterGeneration;
use tracing::warn;

use crate::ThreadStoreError;
use crate::ThreadStoreResult;

const WRITER_LOCK_DIR: &str = "thread-writer-locks";
const COORDINATION_LOCK_FILE: &str = ".coordination.lock";

pub(super) struct WriterLockCoordinator {
    directory: PathBuf,
    cleanup_attempted: AtomicBool,
    owned_threads: Mutex<HashSet<ThreadId>>,
}

pub(super) struct WriterLockGuard {
    coordinator: Arc<WriterLockCoordinator>,
    thread_id: ThreadId,
    path: PathBuf,
    file: Option<File>,
    generation: Option<WriterGeneration>,
}

/// Filesystem ownership observed for one exact thread UUID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadWriterOwnership {
    None,
    OwnedHere,
    OwnedElsewhere,
}

/// Whether persistent writer control metadata is available.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WriterControlCapability {
    Enabled { store_id: String },
    Disabled { reason: String },
}

/// Non-mutating writer authority snapshot for one exact thread UUID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadWriterAuthority {
    pub ownership: ThreadWriterOwnership,
    pub generation: Option<u64>,
    pub control: WriterControlCapability,
}

impl WriterLockCoordinator {
    pub(super) fn new(sqlite_home: &Path) -> Self {
        Self {
            directory: sqlite_home.join(WRITER_LOCK_DIR),
            cleanup_attempted: AtomicBool::new(false),
            owned_threads: Mutex::new(HashSet::new()),
        }
    }

    pub(super) fn acquire(
        self: &Arc<Self>,
        thread_id: ThreadId,
    ) -> ThreadStoreResult<WriterLockGuard> {
        let coordination_lock = self.lock_coordination()?;
        if !self.cleanup_attempted.swap(true, Ordering::Relaxed)
            && let Err(err) = self.remove_stale_thread_locks()
        {
            warn!("failed to clean up stale thread writer locks: {err}");
        }

        let path = self.directory.join(format!("{thread_id}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| ThreadStoreError::Internal {
                message: format!(
                    "failed to open thread writer lock {}: {err}",
                    path.display()
                ),
            })?;

        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ThreadStoreError::Conflict {
                    message: format!("thread {thread_id} already has an active writer"),
                });
            }
            Err(std::fs::TryLockError::Error(err)) => {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "failed to acquire thread writer lock {}: {err}",
                        path.display()
                    ),
                });
            }
        }

        drop(coordination_lock);
        self.owned_threads().insert(thread_id);
        Ok(WriterLockGuard {
            coordinator: Arc::clone(self),
            thread_id,
            path,
            file: Some(file),
            generation: None,
        })
    }

    pub(super) fn probe(&self, thread_id: ThreadId) -> ThreadStoreResult<ThreadWriterOwnership> {
        if self.owned_threads().contains(&thread_id) {
            return Ok(ThreadWriterOwnership::OwnedHere);
        }

        let path = self.directory.join(format!("{thread_id}.lock"));
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(ThreadWriterOwnership::None);
            }
            Err(err) => {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "failed to inspect thread writer lock {}: {err}",
                        path.display()
                    ),
                });
            }
        };
        match file.try_lock() {
            Ok(()) => Ok(ThreadWriterOwnership::None),
            Err(std::fs::TryLockError::WouldBlock) => Ok(ThreadWriterOwnership::OwnedElsewhere),
            Err(std::fs::TryLockError::Error(err)) => Err(ThreadStoreError::Internal {
                message: format!(
                    "failed to inspect thread writer lock {}: {err}",
                    path.display()
                ),
            }),
        }
    }

    fn lock_coordination(&self) -> ThreadStoreResult<File> {
        fs::create_dir_all(&self.directory).map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to create thread writer lock directory {}: {err}",
                self.directory.display()
            ),
        })?;
        let path = self.directory.join(COORDINATION_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| ThreadStoreError::Internal {
                message: format!(
                    "failed to open thread writer coordination lock {}: {err}",
                    path.display()
                ),
            })?;
        file.lock().map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to acquire thread writer coordination lock {}: {err}",
                path.display()
            ),
        })?;
        Ok(file)
    }

    fn owned_threads(&self) -> std::sync::MutexGuard<'_, HashSet<ThreadId>> {
        match self.owned_threads.lock() {
            Ok(owned_threads) => owned_threads,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn remove_stale_thread_locks(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(thread_id) = file_name.strip_suffix(".lock") else {
                continue;
            };
            if ThreadId::from_string(thread_id).is_err() {
                continue;
            }

            let path = entry.path();
            let file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warn!(
                        "failed to inspect thread writer lock {}: {err}",
                        path.display()
                    );
                    continue;
                }
            };
            match file.try_lock() {
                Ok(()) => {
                    drop(file);
                    if let Err(err) = fs::remove_file(&path)
                        && err.kind() != io::ErrorKind::NotFound
                    {
                        warn!(
                            "failed to remove stale thread writer lock {}: {err}",
                            path.display()
                        );
                    }
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(err)) => {
                    warn!(
                        "failed to inspect thread writer lock {}: {err}",
                        path.display()
                    );
                }
            }
        }
        Ok(())
    }
}

impl WriterLockGuard {
    pub(super) fn with_generation(mut self, generation: WriterGeneration) -> Self {
        self.generation = Some(generation);
        self
    }

    pub(super) fn generation(&self) -> Option<u64> {
        self.generation
            .as_ref()
            .map(|generation| generation.generation)
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        self.coordinator.owned_threads().remove(&self.thread_id);
        let coordination_lock = match self.coordinator.lock_coordination() {
            Ok(lock) => lock,
            Err(err) => {
                warn!("failed to coordinate thread writer lock cleanup: {err}");
                return;
            }
        };

        // Close the writer lock before deleting it so cleanup works on Windows too.
        drop(self.file.take());
        if let Err(err) = fs::remove_file(&self.path)
            && err.kind() != io::ErrorKind::NotFound
        {
            warn!(
                "failed to remove thread writer lock {}: {err}",
                self.path.display()
            );
        }
        drop(coordination_lock);
    }
}

#[cfg(test)]
#[path = "writer_lock_tests.rs"]
mod tests;
