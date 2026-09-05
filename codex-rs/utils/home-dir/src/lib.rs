use codex_utils_absolute_path::AbsolutePathBuf;
use dirs::home_dir;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

mod windows_security;
pub use windows_security::ensure_owner_private;
#[cfg(windows)]
pub use windows_security::file_identity;
#[cfg(windows)]
pub use windows_security::file_identity_from_file;
pub use windows_security::is_owner_private;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

const ACCOUNT_REGISTRY_RELATIVE_PATH: &str = ".config/codex-accounts.tsv";
const MAX_ACCOUNT_REGISTRY_BYTES: u64 = 64 * 1024;

/// Returns the path to the Codex configuration directory, which can be
/// specified by the `CODEX_HOME` environment variable. If not set, defaults to
/// `~/.codex`.
///
/// - If `CODEX_HOME` is set, the value must exist and be a directory. The
///   value will be canonicalized and this function will Err otherwise.
/// - If `CODEX_HOME` is not set, this function does not verify that the
///   directory exists.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let codex_home_env = std::env::var("CODEX_HOME")
        .ok()
        .filter(|val| !val.is_empty());
    find_codex_home_from_env(codex_home_env.as_deref())
}

/// Returns the owner home directory without consulting `CODEX_HOME`.
///
/// Owner-scoped services use this path for discovery state that must remain
/// stable while numbered Codex accounts select different configuration homes.
/// Trusted test launchers may explicitly set `CODEX_TEST_OWNER_HOME`, including
/// when exercising release artifacts. It must name an existing absolute
/// directory and is canonicalized before use. An invalid override is an
/// `InvalidInput` error, never a fallback to the real OS owner or unmanaged mode.
/// Without this override, the existing OS owner discovery is unchanged.
pub fn find_owner_home() -> std::io::Result<AbsolutePathBuf> {
    let owner_home_env = std::env::var_os("CODEX_TEST_OWNER_HOME");
    find_owner_home_from_env(owner_home_env.as_deref(), home_dir)
}

fn find_owner_home_from_env(
    owner_home_env: Option<&std::ffi::OsStr>,
    default_owner_home: impl FnOnce() -> Option<PathBuf>,
) -> std::io::Result<AbsolutePathBuf> {
    if let Some(value) = owner_home_env {
        let path = Path::new(value);
        if value.is_empty() || !path.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "CODEX_TEST_OWNER_HOME must be a nonempty absolute directory path",
            ));
        }
        let canonical = path.canonicalize().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("failed to resolve CODEX_TEST_OWNER_HOME: {error}"),
            )
        })?;
        if !canonical.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "CODEX_TEST_OWNER_HOME must be a directory",
            ));
        }
        return AbsolutePathBuf::from_absolute_path(canonical);
    }
    let owner_home = default_owner_home().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not find owner home directory",
        )
    })?;
    AbsolutePathBuf::from_absolute_path(owner_home)
}

/// Canonical logical identifier from the owner account catalog.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ManagedAccountId(u32);

impl ManagedAccountId {
    pub fn parse(value: &str) -> Option<Self> {
        let number = value.strip_prefix('C')?.parse::<u32>().ok()?;
        (number > 0 && value == format!("C{number}")).then_some(Self(number))
    }

    pub const fn from_number(number: u32) -> Option<Self> {
        if number > 0 { Some(Self(number)) } else { None }
    }

    pub const fn number(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for ManagedAccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for ManagedAccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "C{}", self.0)
    }
}

/// Fail-closed reason for rejecting a managed account hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedAccountHintError {
    MalformedHint,
    UnknownAccount,
    UnresolvableCodexHome,
    CodexHomeMismatch,
}

impl fmt::Display for ManagedAccountHintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedHint => f.write_str("managed account hint is malformed"),
            Self::UnknownAccount => f.write_str("managed account is not registered"),
            Self::UnresolvableCodexHome => f.write_str("managed CODEX_HOME cannot be resolved"),
            Self::CodexHomeMismatch => {
                f.write_str("managed account hint does not match CODEX_HOME")
            }
        }
    }
}

impl std::error::Error for ManagedAccountHintError {}

/// Error loading the owner-only managed account catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedAccountCatalogError {
    Unavailable,
    UnsafeFile,
    TooLarge,
    InvalidEncoding,
    InvalidEntry,
    DuplicateEntry,
}

impl fmt::Display for ManagedAccountCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("managed account catalog is unavailable"),
            Self::UnsafeFile => f.write_str("managed account catalog is not owner-only"),
            Self::TooLarge => f.write_str("managed account catalog exceeds its size limit"),
            Self::InvalidEncoding => f.write_str("managed account catalog is not valid UTF-8"),
            Self::InvalidEntry => f.write_str("managed account catalog contains an invalid entry"),
            Self::DuplicateEntry => {
                f.write_str("managed account catalog contains a duplicate entry")
            }
        }
    }
}

impl std::error::Error for ManagedAccountCatalogError {}

/// Owner-local mapping from logical managed account identifiers to canonical
/// Codex homes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManagedAccountCatalog {
    homes: BTreeMap<ManagedAccountId, PathBuf>,
}

impl ManagedAccountCatalog {
    /// Loads `~/.config/codex-accounts.tsv` relative to an explicit owner home.
    ///
    /// The registry must be a regular, non-symlink owner-only file. Malformed
    /// rows, unavailable homes, or duplicate identifiers/homes reject the whole
    /// catalog. C1 must always be registered.
    pub fn load_from_owner_home(owner_home: &Path) -> Result<Self, ManagedAccountCatalogError> {
        let contents = read_account_registry(&owner_home.join(ACCOUNT_REGISTRY_RELATIVE_PATH))?;
        let mut homes = BTreeMap::new();
        let mut unique_homes = HashSet::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split('\t');
            let (Some(number), Some(home), None) = (fields.next(), fields.next(), fields.next())
            else {
                return Err(ManagedAccountCatalogError::InvalidEntry);
            };
            let number = parse_catalog_account_id(number)?;
            let home = catalog_home(home)?;
            if !unique_homes.insert(home.clone()) || homes.insert(number, home).is_some() {
                return Err(ManagedAccountCatalogError::DuplicateEntry);
            }
        }
        let default_account =
            ManagedAccountId::from_number(1).ok_or(ManagedAccountCatalogError::InvalidEntry)?;
        if !homes.contains_key(&default_account) {
            return Err(ManagedAccountCatalogError::InvalidEntry);
        }
        Ok(Self { homes })
    }

    pub fn load() -> Result<Self, ManagedAccountCatalogError> {
        let owner_home = find_owner_home().map_err(|_| ManagedAccountCatalogError::Unavailable)?;
        Self::load_from_owner_home(owner_home.as_path())
    }

    pub fn entries(&self) -> impl Iterator<Item = (ManagedAccountId, &Path)> {
        self.homes
            .iter()
            .map(|(account_id, home)| (*account_id, home.as_path()))
    }

    pub fn home(&self, account_id: ManagedAccountId) -> Option<&Path> {
        self.homes.get(&account_id).map(PathBuf::as_path)
    }

    pub fn account_for_home(&self, codex_home: &Path) -> Option<ManagedAccountId> {
        if !is_clean_absolute_path(codex_home) {
            return None;
        }
        let codex_home = std::fs::canonicalize(codex_home).ok()?;
        self.homes
            .iter()
            .find_map(|(account_id, home)| (home == &codex_home).then_some(*account_id))
    }

    /// Validates a present managed-account hint against the canonical
    /// `CODEX_HOME` mapping. Hint absence is deliberately handled by callers as
    /// the unmanaged upstream path.
    pub fn match_hint(
        &self,
        hint: &str,
        codex_home: &Path,
    ) -> Result<ManagedAccountId, ManagedAccountHintError> {
        let account_id =
            ManagedAccountId::parse(hint).ok_or(ManagedAccountHintError::MalformedHint)?;
        let expected_home = self
            .home(account_id)
            .ok_or(ManagedAccountHintError::UnknownAccount)?;
        if !is_clean_absolute_path(codex_home) {
            return Err(ManagedAccountHintError::UnresolvableCodexHome);
        }
        let codex_home = std::fs::canonicalize(codex_home)
            .map_err(|_| ManagedAccountHintError::UnresolvableCodexHome)?;
        if codex_home != expected_home {
            return Err(ManagedAccountHintError::CodexHomeMismatch);
        }
        Ok(account_id)
    }
}

fn parse_catalog_account_id(value: &str) -> Result<ManagedAccountId, ManagedAccountCatalogError> {
    let number = value
        .parse::<u32>()
        .ok()
        .and_then(ManagedAccountId::from_number)
        .ok_or(ManagedAccountCatalogError::InvalidEntry)?;
    (value == number.number().to_string())
        .then_some(number)
        .ok_or(ManagedAccountCatalogError::InvalidEntry)
}

fn catalog_home(value: &str) -> Result<PathBuf, ManagedAccountCatalogError> {
    let home = PathBuf::from(value);
    if !is_clean_absolute_path(&home) {
        return Err(ManagedAccountCatalogError::InvalidEntry);
    }
    let metadata =
        std::fs::symlink_metadata(&home).map_err(|_| ManagedAccountCatalogError::InvalidEntry)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ManagedAccountCatalogError::InvalidEntry);
    }
    std::fs::canonicalize(home).map_err(|_| ManagedAccountCatalogError::InvalidEntry)
}

/// Returns whether a path is absolute and already in lexical-clean form.
///
/// Canonicalization intentionally remains separate: symlinked ancestors are
/// accepted, while `~`, `.`/`..`, duplicate separators, and trailing separators
/// are rejected before filesystem resolution so callers share one acceptance
/// contract with the TokenManager catalog loader.
fn is_clean_absolute_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }

    let mut rebuilt = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir | Component::ParentDir => return false,
            Component::Prefix(prefix) => rebuilt.push(prefix.as_os_str()),
            Component::RootDir | Component::Normal(_) => rebuilt.push(component.as_os_str()),
        }
    }
    rebuilt == path
}

fn read_account_registry(path: &Path) -> Result<String, ManagedAccountCatalogError> {
    #[cfg(windows)]
    {
        // A registry file controls which credential homes are selected.  Do
        // not trust inherited ACLs: repair only files already owned by this
        // process' user, and reject links before the repair/query.
        let metadata =
            std::fs::symlink_metadata(path).map_err(|_| ManagedAccountCatalogError::Unavailable)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ManagedAccountCatalogError::UnsafeFile);
        }
        let path_identity_before = windows_security::file_identity(path)
            .map_err(|_| ManagedAccountCatalogError::UnsafeFile)?;
        ensure_owner_private(path).map_err(|_| ManagedAccountCatalogError::UnsafeFile)?;
        let protected_identity = windows_security::file_identity(path)
            .map_err(|_| ManagedAccountCatalogError::UnsafeFile)?;
        if path_identity_before != protected_identity {
            return Err(ManagedAccountCatalogError::UnsafeFile);
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    options.custom_flags(0x0000_0100);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(0x0002_0000);
    let mut file = options
        .open(path)
        .map_err(|_| ManagedAccountCatalogError::Unavailable)?;
    let metadata = file
        .metadata()
        .map_err(|_| ManagedAccountCatalogError::Unavailable)?;
    if !metadata.is_file() {
        return Err(ManagedAccountCatalogError::UnsafeFile);
    }
    #[cfg(windows)]
    {
        let opened_identity = windows_security::file_identity_from_file(&file)
            .map_err(|_| ManagedAccountCatalogError::UnsafeFile)?;
        let path_identity = windows_security::file_identity(path)
            .map_err(|_| ManagedAccountCatalogError::UnsafeFile)?;
        if opened_identity != path_identity || !is_owner_private(path).unwrap_or(false) {
            return Err(ManagedAccountCatalogError::UnsafeFile);
        }
    }
    #[cfg(unix)]
    if metadata.mode() & 0o777 != 0o600 || metadata.uid() != effective_uid() {
        return Err(ManagedAccountCatalogError::UnsafeFile);
    }
    if metadata.len() > MAX_ACCOUNT_REGISTRY_BYTES {
        return Err(ManagedAccountCatalogError::TooLarge);
    }
    let mut contents = String::new();
    (&mut file)
        .take(MAX_ACCOUNT_REGISTRY_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|_| ManagedAccountCatalogError::InvalidEncoding)?;
    #[cfg(windows)]
    {
        let after_identity = windows_security::file_identity(path)
            .map_err(|_| ManagedAccountCatalogError::UnsafeFile)?;
        if !is_owner_private(path).unwrap_or(false)
            || windows_security::file_identity_from_file(&file)
                .map_err(|_| ManagedAccountCatalogError::UnsafeFile)?
                != after_identity
        {
            return Err(ManagedAccountCatalogError::UnsafeFile);
        }
    }
    if contents.len() as u64 > MAX_ACCOUNT_REGISTRY_BYTES {
        return Err(ManagedAccountCatalogError::TooLarge);
    }
    Ok(contents)
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: `geteuid` takes no arguments and has no failure mode.
    unsafe { geteuid() }
}

fn find_codex_home_from_env(codex_home_env: Option<&str>) -> std::io::Result<AbsolutePathBuf> {
    // Honor the `CODEX_HOME` environment variable when it is set to allow users
    // (and tests) to override the default location.
    match codex_home_env {
        Some(val) => {
            let path = PathBuf::from(val);
            let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("CODEX_HOME points to {val:?}, but that path does not exist"),
                ),
                _ => std::io::Error::new(
                    err.kind(),
                    format!("failed to read CODEX_HOME {val:?}: {err}"),
                ),
            })?;

            if !metadata.is_dir() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("CODEX_HOME points to {val:?}, but that path is not a directory"),
                ))
            } else {
                let canonical = path.canonicalize().map_err(|err| {
                    std::io::Error::new(
                        err.kind(),
                        format!("failed to canonicalize CODEX_HOME {val:?}: {err}"),
                    )
                })?;
                AbsolutePathBuf::from_absolute_path(canonical)
            }
        }
        None => {
            let mut p = home_dir().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not find home directory",
                )
            })?;
            p.push(".codex");
            AbsolutePathBuf::from_absolute_path(p)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_codex_home_from_env;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use dirs::home_dir;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::io::ErrorKind;
    use tempfile::TempDir;

    #[test]
    fn find_codex_home_env_missing_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(missing_str)).expect_err("missing CODEX_HOME");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(
            err.to_string().contains("CODEX_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_file_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let file_path = temp_home.path().join("codex-home.txt");
        fs::write(&file_path, "not a directory").expect("write temp file");
        let file_str = file_path
            .to_str()
            .expect("file codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(file_str)).expect_err("file CODEX_HOME");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_valid_directory_canonicalizes() {
        let temp_home = TempDir::new().expect("temp home");
        let temp_str = temp_home
            .path()
            .to_str()
            .expect("temp codex home path should be valid utf-8");

        let resolved = find_codex_home_from_env(Some(temp_str)).expect("valid CODEX_HOME");
        let expected = temp_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn find_codex_home_without_env_uses_default_home_dir() {
        let resolved =
            find_codex_home_from_env(/*codex_home_env*/ None).expect("default CODEX_HOME");
        let mut expected = home_dir().expect("home dir");
        expected.push(".codex");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }
}

#[cfg(all(test, unix))]
#[path = "account_catalog_tests.rs"]
mod account_catalog_tests;

#[cfg(test)]
#[path = "owner_home_tests.rs"]
mod owner_home_tests;
