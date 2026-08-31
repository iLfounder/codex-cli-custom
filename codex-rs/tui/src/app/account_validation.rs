//! Post-operation account state validation and notification projections.

use super::account_picker::PendingAccountControl;
use super::*;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeSnapshot;

pub(super) fn revision_meets_lower_bound(actual: u64, minimum: u64) -> bool {
    actual >= minimum
}

pub(super) fn runtime_revision_meets_lower_bound(
    current: Option<(&str, u64)>,
    candidate: (&str, u64),
) -> bool {
    current.is_none_or(|(current_epoch, current_revision)| {
        current_epoch != candidate.0 || revision_meets_lower_bound(candidate.1, current_revision)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountSlotUpdateDisposition {
    Stale,
    Successor,
    Gap,
}

impl App {
    pub(super) fn finish_account_control_validation(&mut self) {
        let Some(pending) = self.pending_account_control.take() else {
            return;
        };
        let valid = match pending {
            PendingAccountControl::Login {
                thread_id,
                target_slot_id,
                instance_epoch,
                attempt_generation,
                minimum_registry_revision,
                ..
            } => {
                let runtime_matches =
                    self.account_runtime
                        .as_ref()
                        .is_some_and(|(epoch, runtime)| {
                            epoch == &instance_epoch && runtime.thread_id == thread_id.to_string()
                        });
                runtime_matches
                    && revision_meets_lower_bound(
                        self.account_registry_revision,
                        minimum_registry_revision,
                    )
                    && self.account_slots.iter().any(|slot| {
                        slot.account_slot_id == target_slot_id
                            && slot.status == AccountSlotStatus::Ready
                            && slot.attempt_generation == attempt_generation
                    })
            }
            PendingAccountControl::Switch {
                thread_id,
                target_slot_id,
                instance_epoch,
                ready_state_revision,
                ready_generation,
                ..
            } => self
                .account_runtime
                .as_ref()
                .is_some_and(|(epoch, runtime)| {
                    epoch == &instance_epoch
                        && runtime.thread_id == thread_id.to_string()
                        && ready_state_revision.is_some_and(|minimum| {
                            revision_meets_lower_bound(runtime.state_revision, minimum)
                        })
                        && runtime.account.switch_state == SessionRuntimeAccountSwitchState::Stable
                        && runtime.account.current.as_ref().is_some_and(|current| {
                            current.account_slot_id == target_slot_id
                                && Some(current.execution_generation) == ready_generation
                        })
                }),
            PendingAccountControl::Logout {
                thread_id,
                target_slot_id,
                instance_epoch,
                minimum_registry_revision,
                prior_generation,
                ..
            } => {
                let runtime_matches =
                    self.account_runtime
                        .as_ref()
                        .is_some_and(|(epoch, runtime)| {
                            epoch == &instance_epoch && runtime.thread_id == thread_id.to_string()
                        });
                runtime_matches
                    && revision_meets_lower_bound(
                        self.account_registry_revision,
                        minimum_registry_revision,
                    )
                    && self.account_slots.iter().any(|slot| {
                        slot.account_slot_id == target_slot_id
                            && slot.status == AccountSlotStatus::LoginRequired
                            && slot.attempt_generation == prior_generation.saturating_add(1)
                    })
            }
        };
        if valid {
            self.chat_widget
                .add_info_message("Account state updated.".to_string(), /*hint*/ None);
        } else {
            self.chat_widget.add_error_message(
                "The refreshed account state did not match the completed operation.".to_string(),
            );
        }
    }

    pub(super) fn handle_account_runtime_changed(
        &mut self,
        instance_epoch: String,
        mut snapshot: SessionRuntimeSnapshot,
    ) {
        if !self.account_rotation_available {
            snapshot.account.rotation = None;
        }
        if self
            .current_displayed_thread_id()
            .is_some_and(|thread_id| snapshot.thread_id == thread_id.to_string())
            && runtime_revision_meets_lower_bound(
                self.account_runtime
                    .as_ref()
                    .map(|(epoch, runtime)| (epoch.as_str(), runtime.state_revision)),
                (instance_epoch.as_str(), snapshot.state_revision),
            )
        {
            self.account_runtime = Some((instance_epoch, snapshot));
        }
    }

    pub(super) fn handle_account_slot_changed(
        &mut self,
        registry_revision: u64,
        slot: AccountSlotSnapshot,
    ) -> AccountSlotUpdateDisposition {
        let selected_slot_id = self.selected_account_slot_id();
        if registry_revision <= self.account_registry_revision {
            return AccountSlotUpdateDisposition::Stale;
        }
        if registry_revision != self.account_registry_revision.saturating_add(1)
            || slot.registry_revision != registry_revision
        {
            return AccountSlotUpdateDisposition::Gap;
        }
        self.account_registry_revision = registry_revision;
        if let Some(existing) = self
            .account_slots
            .iter_mut()
            .find(|existing| existing.account_slot_id == slot.account_slot_id)
        {
            *existing = slot;
        } else {
            self.account_slots.push(slot);
        }
        self.account_slots.sort_by(|left, right| {
            left.account_number
                .cmp(&right.account_number)
                .then_with(|| left.account_slot_id.cmp(&right.account_slot_id))
        });
        self.replace_open_account_views(selected_slot_id.as_deref());
        AccountSlotUpdateDisposition::Successor
    }
}

#[cfg(test)]
#[path = "account_validation_tests.rs"]
mod tests;
