use std::fmt;
use std::fs;
use std::fs::DirBuilder;
use std::io;
use std::path::Path;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

use codex_protocol::auth::AuthMode;

use super::storage::AuthDotJson;
use super::storage::AuthStorageBackend;
use super::storage::CredentialRevision;
use super::storage::FileAuthStorage;
use super::storage::read_auth_file_snapshot;

const STAGING_PREFIX: &str = ".managed-auth-staging.";

/// Secret-free identity and revision of one managed ChatGPT credential file.
#[derive(Clone, Eq, PartialEq)]
pub struct ManagedAuthState {
    account_id: String,
    revision: CredentialRevision,
}

impl ManagedAuthState {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn revision(&self) -> &CredentialRevision {
        &self.revision
    }
}

impl fmt::Debug for ManagedAuthState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedAuthState")
            .field("account_id", &"REDACTED")
            .field("revision", &"REDACTED")
            .finish()
    }
}

/// One temporary credential home colocated with its target for atomic promotion.
pub struct ManagedAuthStaging {
    target_home: PathBuf,
    staging_home: PathBuf,
}

/// Reversible promotion held until the owning lifecycle commits.
pub struct ManagedAuthPromotion {
    target_home: PathBuf,
    previous: Option<AuthDotJson>,
    state: ManagedAuthState,
    accepted: bool,
}

impl fmt::Debug for ManagedAuthPromotion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedAuthPromotion")
            .field("target_home", &"REDACTED")
            .field("state", &self.state)
            .field("accepted", &self.accepted)
            .finish()
    }
}

impl ManagedAuthPromotion {
    pub fn state(&self) -> &ManagedAuthState {
        &self.state
    }

    pub fn accept(mut self) -> ManagedAuthState {
        self.accepted = true;
        self.state.clone()
    }
}

impl Drop for ManagedAuthPromotion {
    fn drop(&mut self) {
        if self.accepted {
            return;
        }
        let storage = FileAuthStorage::new(self.target_home.clone());
        if let Some(previous) = self.previous.as_ref() {
            let _ = storage.save(previous);
        } else {
            let _ = storage.delete();
        }
    }
}

/// Reversible deletion held until the owning lifecycle commits.
pub struct ManagedAuthRemoval {
    target_home: PathBuf,
    previous: AuthDotJson,
    accepted: bool,
}

impl fmt::Debug for ManagedAuthRemoval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedAuthRemoval")
            .field("target_home", &"REDACTED")
            .field("accepted", &self.accepted)
            .finish()
    }
}

impl ManagedAuthRemoval {
    pub fn accept(mut self) {
        self.accepted = true;
    }
}

impl Drop for ManagedAuthRemoval {
    fn drop(&mut self) {
        if !self.accepted {
            let _ = FileAuthStorage::new(self.target_home.clone()).save(&self.previous);
        }
    }
}

impl fmt::Debug for ManagedAuthStaging {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedAuthStaging")
            .field("target_home", &"REDACTED")
            .field("staging_home", &"REDACTED")
            .finish()
    }
}

impl ManagedAuthStaging {
    pub fn create(target_home: &Path) -> io::Result<Self> {
        let target_home = fs::canonicalize(target_home)?;
        if !target_home.is_dir() {
            return Err(io::Error::other("managed credential home is unavailable"));
        }
        #[cfg(windows)]
        codex_utils_home_dir::ensure_owner_private(&target_home)?;
        let staging_home = loop {
            let candidate = target_home.join(format!(
                "{STAGING_PREFIX}{}.{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            #[cfg(unix)]
            let mut builder = DirBuilder::new();
            #[cfg(not(unix))]
            let builder = DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        #[cfg(windows)]
        if let Err(error) = codex_utils_home_dir::ensure_owner_private(&staging_home) {
            let _ = fs::remove_dir_all(&staging_home);
            return Err(error);
        }
        Ok(Self {
            target_home,
            staging_home,
        })
    }

    pub fn home(&self) -> &Path {
        &self.staging_home
    }

    pub fn candidate_state(&self) -> io::Result<ManagedAuthState> {
        managed_auth_state(&self.staging_home)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "managed login did not produce a credential",
            )
        })
    }

    /// Promotes the staged credential only when the target still has the
    /// captured revision and the candidate preserves any established identity.
    pub fn promote(self, expected: Option<&ManagedAuthState>) -> io::Result<ManagedAuthPromotion> {
        let candidate = self.candidate_state()?;
        let current = managed_auth_snapshot(&self.target_home)?;
        let current_state = current.as_ref().map(|(state, _)| state);
        if current_state.map(ManagedAuthState::revision) != expected.map(ManagedAuthState::revision)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "managed credential revision changed",
            ));
        }
        if expected.is_some_and(|state| state.account_id() != candidate.account_id()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed credential identity mismatch",
            ));
        }

        let candidate_auth = read_auth_file_snapshot(&self.staging_home)?
            .ok_or_else(|| io::Error::other("managed credential is unavailable"))?
            .auth;
        FileAuthStorage::new(self.target_home.clone()).save(&candidate_auth)?;
        let state = managed_auth_state(&self.target_home)?.ok_or_else(|| {
            io::Error::other("managed credential promotion could not be verified")
        })?;
        Ok(ManagedAuthPromotion {
            target_home: self.target_home.clone(),
            previous: current.map(|(_, auth)| auth),
            state,
            accepted: false,
        })
    }
}

impl Drop for ManagedAuthStaging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.staging_home);
    }
}

pub fn managed_auth_state(codex_home: &Path) -> io::Result<Option<ManagedAuthState>> {
    managed_auth_snapshot(codex_home).map(|snapshot| snapshot.map(|(state, _)| state))
}

fn managed_auth_snapshot(codex_home: &Path) -> io::Result<Option<(ManagedAuthState, AuthDotJson)>> {
    let Some(snapshot) = read_auth_file_snapshot(codex_home)? else {
        return Ok(None);
    };
    let auth = snapshot.auth;
    if auth.auth_mode.is_some_and(|mode| mode != AuthMode::Chatgpt)
        || auth.openai_api_key.is_some()
        || auth.agent_identity.is_some()
        || auth.personal_access_token.is_some()
        || auth.bedrock_api_key.is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "managed credential type is not allowed",
        ));
    }
    let account_id = auth
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.account_id.clone())
        .filter(|account_id| !account_id.is_empty() && account_id.trim() == account_id)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "managed credential has no stable identity",
            )
        })?;
    Ok(Some((
        ManagedAuthState {
            account_id,
            revision: snapshot.revision,
        },
        auth,
    )))
}

/// Deletes the exact managed credential captured by `expected`.
pub fn remove_managed_auth(
    codex_home: &Path,
    expected: &ManagedAuthState,
) -> io::Result<ManagedAuthRemoval> {
    let Some((current, previous)) = managed_auth_snapshot(codex_home)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "managed credential is unavailable",
        ));
    };
    if &current != expected {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "managed credential revision changed",
        ));
    }
    if !FileAuthStorage::new(codex_home.to_path_buf()).delete()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "managed credential is unavailable",
        ));
    }
    Ok(ManagedAuthRemoval {
        target_home: codex_home.to_path_buf(),
        previous,
        accepted: false,
    })
}

#[cfg(test)]
#[path = "managed_lifecycle_tests.rs"]
mod tests;
