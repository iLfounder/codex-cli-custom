use codex_backend_client::ApiKeyTurnCost;
use codex_backend_client::ApiKeyTurnCostStatus;
use codex_backend_client::Client as BackendClient;
use codex_config::types::OtelExporterKind;
use codex_core::config::Config;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const POLL_INTERVAL: Duration = Duration::from_secs(150);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const OBSERVATION_CHANNEL_CAPACITY: usize = 16_384;
const MAX_TRACKED_TURNS: usize = 4_096;
const MAX_QUERY_TURNS: usize = 100;
const MAX_STALLED_POLL_ATTEMPTS: u8 = 5;

pub(crate) struct TurnCostWorker {
    handle: TurnCostWorkerHandle,
    shutdown: CancellationToken,
    _task: JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct TurnCostWorkerHandle {
    sender: mpsc::Sender<TurnCostObservation>,
}

enum TurnCostObservationKind {
    Started {
        session_telemetry: Box<SessionTelemetry>,
    },
    ResponseCompleted,
    Finished {
        interrupted: bool,
    },
}

struct TurnCostObservation {
    thread_id: ThreadId,
    turn_id: String,
    auth_manager: Arc<AuthManager>,
    kind: TurnCostObservationKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnCostStatus {
    Running,
    Completed,
    Interrupted,
}

struct TurnCostEntry {
    thread_id: ThreadId,
    auth_manager: Arc<AuthManager>,
    session_telemetry: SessionTelemetry,
    expected_response_count: u64,
    status: TurnCostStatus,
    next_poll_at: Instant,
    attempt_count: u8,
}

struct WorkerRuntime {
    config: Arc<Config>,
    turns: HashMap<String, TurnCostEntry>,
}

impl TurnCostWorker {
    pub(crate) fn spawn(config: Arc<Config>) -> Option<Self> {
        if !matches!(
            config.otel.exporter,
            OtelExporterKind::OtlpHttp { .. } | OtelExporterKind::OtlpGrpc { .. }
        ) || !config.model_provider.is_openai()
        {
            return None;
        }
        let (sender, receiver) = mpsc::channel(OBSERVATION_CHANNEL_CAPACITY);
        let shutdown = CancellationToken::new();
        let runtime = WorkerRuntime {
            config: Arc::clone(&config),
            turns: HashMap::new(),
        };
        let worker_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            runtime.run(receiver, worker_shutdown).await;
        });
        Some(Self {
            handle: TurnCostWorkerHandle { sender },
            shutdown,
            _task: task,
        })
    }

    pub(crate) fn handle(&self) -> TurnCostWorkerHandle {
        self.handle.clone()
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

impl Drop for TurnCostWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl TurnCostWorkerHandle {
    pub(crate) fn observe_event(
        &self,
        thread_id: ThreadId,
        event: &Event,
        auth_manager: Arc<AuthManager>,
        session_telemetry: impl FnOnce() -> SessionTelemetry,
    ) {
        let kind = match &event.msg {
            EventMsg::TurnStarted(_) => {
                let Some(auth) = auth_manager.auth_cached() else {
                    return;
                };
                if !auth.is_api_key_auth() {
                    return;
                }
                TurnCostObservationKind::Started {
                    session_telemetry: Box::new(session_telemetry()),
                }
            }
            EventMsg::RawResponseCompleted(_) => TurnCostObservationKind::ResponseCompleted,
            EventMsg::TurnComplete(_) => TurnCostObservationKind::Finished { interrupted: false },
            EventMsg::TurnAborted(_) => TurnCostObservationKind::Finished { interrupted: true },
            _ => return,
        };
        let _ = self.sender.try_send(TurnCostObservation {
            thread_id,
            turn_id: event.id.clone(),
            auth_manager,
            kind,
        });
    }
}

impl WorkerRuntime {
    async fn run(
        mut self,
        mut receiver: mpsc::Receiver<TurnCostObservation>,
        shutdown: CancellationToken,
    ) {
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                observation = receiver.recv() => {
                    let Some(observation) = observation else {
                        break;
                    };
                    self.record_observation(observation);
                }
                _ = ticker.tick() => self.poll_due().await,
            }
        }
    }

    fn record_observation(&mut self, observation: TurnCostObservation) {
        match observation.kind {
            TurnCostObservationKind::Started { session_telemetry } => {
                if self.turns.len() < MAX_TRACKED_TURNS {
                    self.turns
                        .entry(observation.turn_id)
                        .or_insert(TurnCostEntry {
                            thread_id: observation.thread_id,
                            auth_manager: observation.auth_manager,
                            session_telemetry: *session_telemetry,
                            expected_response_count: 0,
                            status: TurnCostStatus::Running,
                            next_poll_at: Instant::now(),
                            attempt_count: 0,
                        });
                }
            }
            TurnCostObservationKind::ResponseCompleted => {
                if let Some(entry) = self.turns.get_mut(&observation.turn_id)
                    && entry.status == TurnCostStatus::Running
                {
                    entry.expected_response_count = entry.expected_response_count.saturating_add(1);
                }
            }
            TurnCostObservationKind::Finished { interrupted } => {
                let Some(entry) = self.turns.get_mut(&observation.turn_id) else {
                    return;
                };
                if entry.status != TurnCostStatus::Running {
                    return;
                }
                entry.status = if interrupted {
                    TurnCostStatus::Interrupted
                } else {
                    TurnCostStatus::Completed
                };
                entry.next_poll_at = Instant::now();
            }
        }
    }

    async fn poll_due(&mut self) {
        let now = Instant::now();
        let mut due_by_account: HashMap<usize, (Arc<AuthManager>, Vec<String>)> = HashMap::new();
        for (turn_id, entry) in self.turns.iter().filter(|(_, entry)| {
            entry.status != TurnCostStatus::Running && entry.next_poll_at <= now
        }) {
            let account_key = Arc::as_ptr(&entry.auth_manager) as usize;
            let (_, turn_ids) = due_by_account
                .entry(account_key)
                .or_insert_with(|| (Arc::clone(&entry.auth_manager), Vec::new()));
            if turn_ids.len() < MAX_QUERY_TURNS {
                turn_ids.push(turn_id.clone());
            }
        }
        for (_, (auth_manager, turn_ids)) in due_by_account {
            let Some(auth) = auth_manager.auth().await else {
                self.retry_entries(&turn_ids);
                continue;
            };
            if !auth.is_api_key_auth() {
                self.retry_entries(&turn_ids);
                continue;
            }
            self.poll_api_key_entries(&turn_ids, &auth).await;
        }
    }

    async fn poll_api_key_entries(&mut self, turn_ids: &[String], auth: &CodexAuth) {
        let provider = match self
            .config
            .model_provider
            .to_api_provider(Some(AuthMode::ApiKey))
        {
            Ok(provider) => provider,
            Err(error) => {
                warn!("failed to resolve OpenAI API-key provider headers: {error}");
                self.retry_entries(turn_ids);
                return;
            }
        };
        let client = BackendClient::from_auth(
            self.config.chatgpt_base_url.clone(),
            auth,
            self.config.http_client_factory(),
        );
        let costs = match tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.query_api_key_turn_costs(turn_ids, &provider.headers),
        )
        .await
        {
            Ok(Ok(costs)) => costs,
            Ok(Err(error)) => {
                warn!("failed to query OpenAI API-key turn costs: {error}");
                self.retry_entries(turn_ids);
                return;
            }
            Err(_) => {
                warn!("timed out querying OpenAI API-key turn costs");
                self.retry_entries(turn_ids);
                return;
            }
        };
        let costs_by_turn: HashMap<String, ApiKeyTurnCost> = costs
            .into_iter()
            .map(|cost| (cost.turn_id.clone(), cost))
            .collect();
        for turn_id in turn_ids {
            let Some(cost) = costs_by_turn.get(turn_id) else {
                self.retry_entry(turn_id);
                continue;
            };
            self.process_api_key_cost(turn_id, cost);
        }
    }

    fn process_api_key_cost(&mut self, turn_id: &str, cost: &ApiKeyTurnCost) {
        if cost.status != ApiKeyTurnCostStatus::Priced {
            self.retry_entry(turn_id);
            return;
        }
        let response_count = cost
            .responses
            .as_ref()
            .map(|responses| responses.len() as u64)
            .or(cost.event_count);
        let (Some(total_usd), Some(response_count)) = (cost.total_usd.as_deref(), response_count)
        else {
            self.retry_entry(turn_id);
            return;
        };
        let Some(entry) = self.turns.get(turn_id) else {
            return;
        };
        if response_count < entry.expected_response_count {
            self.retry_entry(turn_id);
            return;
        }
        let mut session_telemetry = entry.session_telemetry.clone();
        if let Some(model) = cost.model.as_deref() {
            session_telemetry = session_telemetry.with_model(model, model);
        }
        let Some(entry) = self.turns.remove(turn_id) else {
            return;
        };
        session_telemetry.record_turn_cost(
            turn_id,
            total_usd,
            entry.status == TurnCostStatus::Interrupted,
            cost.speed.as_deref(),
            cost.reasoning_effort.as_deref(),
        );
    }

    fn retry_entries(&mut self, turn_ids: &[String]) {
        for turn_id in turn_ids {
            self.retry_entry(turn_id);
        }
    }

    fn retry_entry(&mut self, turn_id: &str) {
        let Some(entry) = self.turns.get_mut(turn_id) else {
            return;
        };
        entry.attempt_count = entry.attempt_count.saturating_add(1);
        if entry.attempt_count >= MAX_STALLED_POLL_ATTEMPTS {
            warn!(
                thread_id = %entry.thread_id,
                turn_id,
                attempts = MAX_STALLED_POLL_ATTEMPTS,
                "dropping turn cost event after repeated unsuccessful polls"
            );
            self.turns.remove(turn_id);
            return;
        }
        entry.next_poll_at = Instant::now() + POLL_INTERVAL;
    }
}

#[cfg(test)]
#[path = "turn_cost_worker_tests.rs"]
mod tests;
