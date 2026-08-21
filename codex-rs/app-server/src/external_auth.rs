use std::sync::Arc;
use std::sync::RwLock;

use codex_app_server_protocol::ChatgptAuthTokensRefreshParams;
use codex_app_server_protocol::ChatgptAuthTokensRefreshReason;
use codex_app_server_protocol::ChatgptAuthTokensRefreshResponse;
use codex_app_server_protocol::ServerRequestPayload;
use codex_login::CodexAuth;
use codex_login::ExternalAuthFuture;
use codex_login::auth::ExternalAuth;
use codex_login::auth::ExternalAuthRefreshContext;
use codex_login::auth::ExternalAuthRefreshReason;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;

const EXTERNAL_AUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct ExternalAuthBridge {
    outgoing: Arc<OutgoingMessageSender>,
    auth: RwLock<CodexAuth>,
    owner_connection: Option<ConnectionId>,
    refresh_unavailable: Option<CancellationToken>,
}

impl ExternalAuthBridge {
    pub(crate) fn new(outgoing: Arc<OutgoingMessageSender>, auth: CodexAuth) -> Self {
        Self {
            outgoing,
            auth: RwLock::new(auth),
            owner_connection: None,
            refresh_unavailable: None,
        }
    }

    pub(crate) fn new_for_connection(
        outgoing: Arc<OutgoingMessageSender>,
        auth: CodexAuth,
        owner_connection: ConnectionId,
        refresh_unavailable: CancellationToken,
    ) -> Self {
        Self {
            outgoing,
            auth: RwLock::new(auth),
            owner_connection: Some(owner_connection),
            refresh_unavailable: Some(refresh_unavailable),
        }
    }

    async fn refresh(&self, context: ExternalAuthRefreshContext) -> std::io::Result<CodexAuth> {
        let result = self.refresh_inner(context).await;
        if result.is_err()
            && let Some(refresh_unavailable) = self.refresh_unavailable.as_ref()
        {
            refresh_unavailable.cancel();
        }
        result
    }

    async fn refresh_inner(
        &self,
        context: ExternalAuthRefreshContext,
    ) -> std::io::Result<CodexAuth> {
        let reason = match context.reason {
            ExternalAuthRefreshReason::Unauthorized => ChatgptAuthTokensRefreshReason::Unauthorized,
        };
        let params = ChatgptAuthTokensRefreshParams {
            reason,
            previous_account_id: context.previous_account_id,
        };

        let payload = ServerRequestPayload::ChatgptAuthTokensRefresh(params);
        let (request_id, rx) = match self.owner_connection {
            Some(connection_id) => {
                self.outgoing
                    .send_request_to_connections(
                        Some(&[connection_id]),
                        payload,
                        /*thread_id*/ None,
                    )
                    .await
            }
            None => self.outgoing.send_request(payload).await,
        };
        let result = match timeout(EXTERNAL_AUTH_REFRESH_TIMEOUT, rx).await {
            Ok(result) => {
                let result = result.map_err(|err| {
                    std::io::Error::other(format!("auth refresh request canceled: {err}"))
                })?;
                result.map_err(|err| {
                    std::io::Error::other(format!(
                        "auth refresh request failed: code={} message={}",
                        err.code, err.message
                    ))
                })?
            }
            Err(_) => {
                let _canceled = self.outgoing.cancel_request(&request_id).await;
                return Err(std::io::Error::other(format!(
                    "auth refresh request timed out after {}s",
                    EXTERNAL_AUTH_REFRESH_TIMEOUT.as_secs()
                )));
            }
        };

        let response: ChatgptAuthTokensRefreshResponse =
            serde_json::from_value(result).map_err(std::io::Error::other)?;
        let auth = CodexAuth::from_external_chatgpt_tokens(
            response.access_token.as_str(),
            response.chatgpt_account_id.as_str(),
            response.chatgpt_plan_type.as_deref(),
        )?;
        *self
            .auth
            .write()
            .map_err(|_| std::io::Error::other("external auth lock is poisoned"))? = auth.clone();
        Ok(auth)
    }
}

impl ExternalAuth for ExternalAuthBridge {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async {
            self.auth
                .read()
                .map(|auth| auth.clone())
                .map_err(|_| std::io::Error::other("external auth lock is poisoned"))
        })
    }

    fn refresh(&self, context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(ExternalAuthBridge::refresh(self, context))
    }
}
