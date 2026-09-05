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
                    metadata.active_user_git.extend(
                        entries
                            .iter()
                            .filter(|&(_name, config)| {
                                config
                                    .get("source_type")
                                    .and_then(serde_json::Value::as_str)
                                    == Some("git")
                            })
                            .map(|(name, _config)| name.clone()),
                    );
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
            params: ConfigReadParams {
                include_layers: true,
                cwd: Some(cwd),
            },
        })
        .await
        .map_err(|_| {
            "Could not read remote marketplace management settings. Refresh /plugins to retry."
                .to_string()
        })?;
    let layers = response
        .layers
        .ok_or_else(|| "The server did not return marketplace configuration layers.".to_string())?;
    Ok(MarketplaceManagement::from_layers(&layers))
}

#[cfg(test)]
#[path = "marketplace_management_tests.rs"]
mod tests;
