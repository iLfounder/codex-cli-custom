use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_app_server_transport::ManagedAccountCatalog;
use codex_app_server_transport::find_owner_home;
use sha2::Digest;
use sha2::Sha256;

use super::AccountId;

const SOURCE_REF_CONTEXT: &str = "llm-bridge.subscription-source-ref/v1";
const SOURCE_REF_PREFIX: &str = "subscription-source-v1:";

#[derive(Clone, Default)]
pub(crate) struct GlobalAccountDirectory {
    pub(crate) homes: BTreeMap<AccountId, PathBuf>,
    pub(crate) process_account_id: Option<AccountId>,
}

impl GlobalAccountDirectory {
    pub(crate) fn user_home() -> Option<PathBuf> {
        find_owner_home().ok().map(Into::into)
    }

    pub(crate) fn load_from(user_home: &Path, process_home: &Path) -> Self {
        let catalog = ManagedAccountCatalog::load_from_owner_home(user_home).unwrap_or_default();
        let homes = catalog
            .entries()
            .filter_map(|(account_id, home)| {
                AccountId::parse(&account_id.to_string())
                    .map(|account_id| (account_id, home.to_path_buf()))
            })
            .collect::<BTreeMap<_, _>>();
        let process_account_id = catalog
            .account_for_home(process_home)
            .and_then(|account_id| AccountId::parse(&account_id.to_string()))
            .or_else(|| {
                let process_home = std::fs::canonicalize(process_home).ok()?;
                let default_home = std::fs::canonicalize(user_home.join(".codex")).ok()?;
                if process_home != default_home {
                    return None;
                }
                catalog.entries().find_map(|(account_id, _)| {
                    (account_id.number() == 1)
                        .then(|| AccountId::parse(&account_id.to_string()))
                        .flatten()
                })
            });
        Self {
            homes,
            process_account_id,
        }
    }

    pub(crate) fn inventory_fingerprint(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"codex-account-inventory/v1\0");
        for (account_id, home) in &self.homes {
            digest.update(account_id.number().to_be_bytes());
            digest.update(b"\0");
            digest.update(home.to_string_lossy().as_bytes());
            digest.update(b"\0");
        }
        digest.finalize().into()
    }
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
