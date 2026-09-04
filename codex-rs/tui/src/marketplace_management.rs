//! Remote marketplace management comes from the server's user layers, never the TUI's config.
//! Keep only action metadata; discard the rest of the returned configuration after projection.

use std::collections::HashSet;

use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigLayer;
use codex_app_server_protocol::ConfigLayerSource;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::RequestId;
use codex_utils_path_uri::LegacyAppPathString;
use uuid::Uuid;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MarketplaceManagement {
    user_configured: HashSet<String>,
    active_user_git: HashSet<String>,
}

impl MarketplaceManagement {
    /// Mirror effective_user_config for removal and get_active_user_layer for upgrades.
    pub(crate) fn from_layers(layers: &[ConfigLayer]) -> Self {
        let mut metadata = Self::default();
        // config/read returns highest precedence first.
        for layer in layers.iter().rev() {
            if !matches!(layer.name, ConfigLayerSource::User { .. }) {
                continue;
            }
            // The writable profile is the last raw user layer, not the merged config.
            metadata.active_user_git.clear();
            if let Some(marketplaces) = layer.config.get("marketplaces") {
                if let Some(entries) = marketplaces.as_object() {
                    metadata.active_user_git.extend(entries.iter().filter_map(|(name, config)| {
                        (config.get("source_type").and_then(serde_json::Value::as_str) == Some("git"))
                            .then(|| name.clone())
                    }));
                }
                if layer.disabled_reason.is_none() {
                    if let Some(entries) = marketplaces.as_object() {
                        metadata.user_configured.extend(entries.keys().cloned());
                    } else {
                        metadata.user_configured.clear();
                    }
                }
            }
        }
        metadata
    }

    pub(crate) fn is_user_configured(&self, name: &str) -> bool {
        self.user_configured.contains(name)
    }

    pub(crate) fn is_user_configured_git(&self, name: &str) -> bool {
        self.active_user_git.contains(name)
    }
}

pub(crate) async fn fetch_marketplace_management(
    request_handle: AppServerRequestHandle,
    cwd: LegacyAppPathString,
) -> Result<MarketplaceManagement, String> {
    let response: ConfigReadResponse = request_handle
        .request_typed(ClientRequest::ConfigRead {
            request_id: RequestId::String(format!("marketplace-management-{}", Uuid::new_v4())),
            params: ConfigReadParams { include_layers: true, cwd: Some(cwd) },
        })
        .await
        .map_err(|_| "Could not read remote marketplace management settings. Refresh /plugins to retry.".to_string())?;
    let layers = response.layers.ok_or_else(|| "The server did not return marketplace configuration layers.".to_string())?;
    Ok(MarketplaceManagement::from_layers(&layers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn user_layer(profile: Option<&str>, config: serde_json::Value) -> ConfigLayer {
        ConfigLayer {
            name: ConfigLayerSource::User {
                file: LegacyAppPathString::from_string("/server/codex/config.toml"),
                profile: profile.map(str::to_string),
            },
            version: "test".to_string(),
            config,
            disabled_reason: None,
        }
    }

    #[test]
    fn remote_management_merges_enabled_user_names_but_upgrades_only_active_profile_git() {
        let mut system = user_layer(None, json!({"marketplaces": {"managed": {"source_type": "git"}}}));
        system.name = ConfigLayerSource::System { file: LegacyAppPathString::from_string("/etc/codex/config.toml") };
        let mut disabled = user_layer(None, json!({"marketplaces": {"disabled": {"source_type": "git"}}}));
        disabled.disabled_reason = Some("disabled fixture".to_string());
        let metadata = MarketplaceManagement::from_layers(&[
            user_layer(Some("work"), json!({"marketplaces": {"profile": {"source_type": "git"}, "local": {"source_type": "local"}}})),
            disabled,
            user_layer(None, json!({"marketplaces": {"base": {"source_type": "git"}}})),
            system,
        ]);
        assert_eq!(metadata.user_configured, HashSet::from(["base".to_string(), "profile".to_string(), "local".to_string()]));
        assert_eq!(metadata.active_user_git, HashSet::from(["profile".to_string()]));
    }

    #[test]
    fn empty_active_profile_does_not_inherit_upgrade_authority() {
        let metadata = MarketplaceManagement::from_layers(&[
            user_layer(Some("work"), json!({})),
            user_layer(None, json!({"marketplaces": {"base": {"source_type": "git"}}})),
        ]);
        assert!(metadata.is_user_configured("base"));
        assert!(!metadata.is_user_configured_git("base"));
        assert!(!metadata.is_user_configured("windows-only"));
    }
}
