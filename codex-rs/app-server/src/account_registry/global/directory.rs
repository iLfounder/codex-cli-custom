use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::Digest;
use sha2::Sha256;

use super::AccountId;

const ACCOUNT_REGISTRY_FILE: &str = ".config/codex-accounts.tsv";
const SOURCE_REF_CONTEXT: &str = "llm-bridge.subscription-source-ref/v1";
const SOURCE_REF_PREFIX: &str = "subscription-source-v1:";

#[derive(Clone, Default)]
pub(crate) struct GlobalAccountDirectory {
    pub(crate) homes: BTreeMap<AccountId, PathBuf>,
    pub(crate) process_account_id: Option<AccountId>,
}

impl GlobalAccountDirectory {
    pub(crate) fn user_home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    pub(crate) fn load_from(user_home: &Path, process_home: &Path) -> Self {
        let registry_path = user_home.join(ACCOUNT_REGISTRY_FILE);
        let Some(contents) = read_registry(&registry_path) else {
            return Self::default();
        };
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
                return Self::default();
            };
            let Ok(number) = number.parse::<u32>() else {
                return Self::default();
            };
            let Some(account_id) = AccountId::parse(&format!("C{number}")) else {
                return Self::default();
            };
            let home = match home.strip_prefix("~/") {
                Some(relative) => user_home.join(relative),
                None => PathBuf::from(home),
            };
            let Ok(home) = std::fs::canonicalize(home) else {
                continue;
            };
            if !unique_homes.insert(home.clone()) || homes.insert(account_id, home).is_some() {
                return Self::default();
            }
        }
        let process_home = std::fs::canonicalize(process_home).ok();
        let process_account_id = process_home.as_ref().and_then(|process_home| {
            homes
                .iter()
                .find_map(|(account_id, home)| (home == process_home).then_some(*account_id))
        });
        Self {
            homes,
            process_account_id,
        }
    }
}

fn read_registry(path: &Path) -> Option<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    options.custom_flags(0x0000_0100);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(0x0002_0000);
    let mut file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    if metadata.mode() & 0o777 != 0o600 || metadata.uid() != effective_uid() {
        return None;
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    Some(contents)
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: `geteuid` takes no arguments and has no failure mode.
    unsafe { geteuid() }
}

pub(crate) fn subscription_source_ref(
    account_identity: &str,
    canonical_home: &Path,
) -> Option<String> {
    let canonical_home = canonical_home.to_str()?;
    if account_identity.is_empty() || !Path::new(canonical_home).is_absolute() {
        return None;
    }
    let mut digest = Sha256::new();
    digest.update(SOURCE_REF_CONTEXT.as_bytes());
    digest.update(b"\0codex-cli\0");
    digest.update(account_identity.as_bytes());
    digest.update(b"\0");
    digest.update(canonical_home.as_bytes());
    Some(format!(
        "{SOURCE_REF_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(digest.finalize())
    ))
}

#[cfg(test)]
#[path = "directory_tests.rs"]
mod tests;
