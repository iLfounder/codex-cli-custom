use codex_app_server_protocol::ThreadTokenUsage;

use crate::exec_events::ContextCompactionItem;
use crate::exec_events::ContextCompactionStatus;
use crate::exec_events::ContextCompactionUsage;
use crate::exec_events::ItemCompletedEvent;
use crate::exec_events::ItemStartedEvent;
use crate::exec_events::ItemUpdatedEvent;
use crate::exec_events::ThreadEvent;
use crate::exec_events::ThreadItem;
use crate::exec_events::ThreadItemDetails;
use crate::exec_events::TokenUsageBreakdown;

#[derive(Debug, Default)]
pub(super) struct CompactionObserver {
    active: Option<ActiveCompaction>,
    latest_turn_usage: Option<ObservedTurnUsage>,
}

#[derive(Debug)]
struct ActiveCompaction {
    raw_id: String,
    exec_id: String,
    thread_id: String,
    turn_id: String,
    started_at_ms: i64,
    before: Option<ContextCompactionUsage>,
    latest_reported: Option<ContextCompactionUsage>,
}

#[derive(Debug)]
struct ObservedTurnUsage {
    thread_id: String,
    turn_id: String,
    usage: ContextCompactionUsage,
}

impl CompactionObserver {
    pub(super) fn start(
        &mut self,
        raw_id: String,
        exec_id: String,
        thread_id: String,
        turn_id: String,
        started_at_ms: i64,
    ) -> Vec<ThreadEvent> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.raw_id == raw_id)
        {
            return Vec::new();
        }

        let mut events: Vec<ThreadEvent> =
            self.close_active_as_outcome_unknown().into_iter().collect();
        let before = self
            .latest_turn_usage
            .as_ref()
            .filter(|usage| usage.thread_id == thread_id && usage.turn_id == turn_id)
            .map(|usage| usage.usage.clone());
        let active = ActiveCompaction {
            raw_id,
            exec_id,
            thread_id,
            turn_id,
            started_at_ms,
            before,
            latest_reported: None,
        };
        events.push(ThreadEvent::ItemStarted(ItemStartedEvent {
            item: active.item(
                ContextCompactionStatus::Compacting,
                /*completed_at_ms*/ None,
            ),
        }));
        self.active = Some(active);
        events
    }

    pub(super) fn observe_token_usage(
        &mut self,
        thread_id: String,
        turn_id: String,
        token_usage: ThreadTokenUsage,
    ) -> Option<ThreadEvent> {
        let usage = map_token_usage(token_usage);
        self.latest_turn_usage = Some(ObservedTurnUsage {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            usage: usage.clone(),
        });

        let active = self.active.as_mut()?;
        if active.thread_id != thread_id
            || active.turn_id != turn_id
            || active.latest_reported.as_ref() == Some(&usage)
        {
            return None;
        }

        active.latest_reported = Some(usage);
        Some(ThreadEvent::ItemUpdated(ItemUpdatedEvent {
            item: active.item(
                ContextCompactionStatus::Compacting,
                /*completed_at_ms*/ None,
            ),
        }))
    }

    pub(super) fn complete(
        &mut self,
        raw_id: &str,
        completed_at_ms: i64,
        missing_start_exec_id: impl FnOnce() -> String,
    ) -> ThreadEvent {
        let item = match self.active.take() {
            Some(active) if active.raw_id == raw_id => {
                active.item(ContextCompactionStatus::Completed, Some(completed_at_ms))
            }
            Some(active) => {
                self.active = Some(active);
                ThreadItem {
                    id: missing_start_exec_id(),
                    details: ThreadItemDetails::ContextCompaction(ContextCompactionItem {
                        status: ContextCompactionStatus::Completed,
                        started_at_ms: None,
                        completed_at_ms: Some(completed_at_ms),
                        duration_ms: None,
                        before: None,
                        latest_reported: None,
                    }),
                }
            }
            None => ThreadItem {
                id: missing_start_exec_id(),
                details: ThreadItemDetails::ContextCompaction(ContextCompactionItem {
                    status: ContextCompactionStatus::Completed,
                    started_at_ms: None,
                    completed_at_ms: Some(completed_at_ms),
                    duration_ms: None,
                    before: None,
                    latest_reported: None,
                }),
            },
        };
        ThreadEvent::ItemCompleted(ItemCompletedEvent { item })
    }

    pub(super) fn turn_completed(&mut self, thread_id: &str, turn_id: &str) -> Option<ThreadEvent> {
        if !self
            .active
            .as_ref()
            .is_some_and(|active| active.thread_id == thread_id && active.turn_id == turn_id)
        {
            return None;
        }
        self.close_active_as_outcome_unknown()
    }

    pub(super) fn shutdown(&mut self) -> Option<ThreadEvent> {
        self.close_active_as_outcome_unknown()
    }

    fn close_active_as_outcome_unknown(&mut self) -> Option<ThreadEvent> {
        let active = self.active.take()?;
        Some(ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: active.item(
                ContextCompactionStatus::OutcomeUnknown,
                /*completed_at_ms*/ None,
            ),
        }))
    }
}

impl ActiveCompaction {
    fn item(&self, status: ContextCompactionStatus, completed_at_ms: Option<i64>) -> ThreadItem {
        ThreadItem {
            id: self.exec_id.clone(),
            details: ThreadItemDetails::ContextCompaction(ContextCompactionItem {
                status,
                started_at_ms: Some(self.started_at_ms),
                completed_at_ms,
                duration_ms: completed_at_ms
                    .and_then(|completed_at_ms| completed_at_ms.checked_sub(self.started_at_ms)),
                before: self.before.clone(),
                latest_reported: self.latest_reported.clone(),
            }),
        }
    }
}

fn map_token_usage(value: ThreadTokenUsage) -> ContextCompactionUsage {
    ContextCompactionUsage {
        reported_last_usage: TokenUsageBreakdown {
            total_tokens: value.last.total_tokens,
            input_tokens: value.last.input_tokens,
            cached_input_tokens: value.last.cached_input_tokens,
            cache_write_input_tokens: value.last.cache_write_input_tokens,
            output_tokens: value.last.output_tokens,
            reasoning_output_tokens: value.last.reasoning_output_tokens,
        },
        reported_total_usage: TokenUsageBreakdown {
            total_tokens: value.total.total_tokens,
            input_tokens: value.total.input_tokens,
            cached_input_tokens: value.total.cached_input_tokens,
            cache_write_input_tokens: value.total.cache_write_input_tokens,
            output_tokens: value.total.output_tokens,
            reasoning_output_tokens: value.total.reasoning_output_tokens,
        },
        model_context_window: value.model_context_window,
    }
}

#[cfg(test)]
#[path = "compaction_observer_tests.rs"]
mod tests;
