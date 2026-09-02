//! Pure projection of accepted account/runtime state into display-safe footer values.

use super::App;
use crate::bottom_pane::FooterRuntimeProjection;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::AccountSlotHealth;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_app_server_protocol::ThreadAccountRotationMode;

pub(super) fn project_footer_runtime(
    displayed_thread_id: Option<&str>,
    runtime: Option<&SessionRuntimeSnapshot>,
    slot_capability: Option<&AccountSlotCapability>,
    slots: &[AccountSlotSnapshot],
) -> FooterRuntimeProjection {
    let Some(runtime) = runtime.filter(|runtime| {
        displayed_thread_id.is_some_and(|thread_id| runtime.thread_id == thread_id)
    }) else {
        return FooterRuntimeProjection::default();
    };

    let mut projection = FooterRuntimeProjection {
        session_id: Some(runtime.identity.session_id.clone()),
        thread_id: Some(runtime.thread_id.clone()),
        thread_name: runtime.identity.name.clone(),
        runtime_state: Some(
            match runtime.lifecycle.state {
                SessionRuntimeLifecycleState::NotLoaded => "not loaded",
                SessionRuntimeLifecycleState::Loaded => "loaded",
                SessionRuntimeLifecycleState::Idle => "idle",
                SessionRuntimeLifecycleState::Active => "active",
                SessionRuntimeLifecycleState::Closing => "closing",
            }
            .to_string(),
        ),
        rotation_state: Some(
            match runtime
                .account
                .rotation
                .as_ref()
                .map(|rotation| rotation.mode)
            {
                Some(ThreadAccountRotationMode::Fixed) => "fixed · switch ",
                Some(ThreadAccountRotationMode::QuotaAware) => "quota aware · switch ",
                Some(ThreadAccountRotationMode::RoundRobin) => "round robin · switch ",
                Some(ThreadAccountRotationMode::ExhaustThenNext) => "exhaust then next · switch ",
                None => "switch ",
            }
            .to_string()
                + match runtime.account.switch_state {
                    SessionRuntimeAccountSwitchState::Stable => "stable",
                    SessionRuntimeAccountSwitchState::Preparing => "preparing",
                    SessionRuntimeAccountSwitchState::Switching => "switching",
                    SessionRuntimeAccountSwitchState::Unbound => "unbound",
                    SessionRuntimeAccountSwitchState::Degraded => "degraded",
                },
        ),
        ..FooterRuntimeProjection::default()
    };

    let Some(current) = runtime.account.current.as_ref() else {
        return projection;
    };
    let Some(slot) = slot_capability.and_then(|_| {
        slots
            .iter()
            .find(|slot| slot.account_slot_id == current.account_slot_id)
    }) else {
        return projection;
    };

    projection.managed_slot_label = Some(slot.account_number.to_string());
    projection.managed_slot_id = Some(slot.account_slot_id.clone());
    projection.managed_slot_health = Some(
        match slot.health {
            AccountSlotHealth::Healthy => "healthy",
            AccountSlotHealth::Degraded => "degraded",
            AccountSlotHealth::Unavailable => "unavailable",
        }
        .to_string(),
    );
    projection.managed_slot_quota = slot.quota.as_ref().and_then(|quota| {
        let meters = quota
            .meters
            .iter()
            .map(|meter| {
                format!(
                    "{} {}%",
                    meter.label.as_deref().unwrap_or(&meter.id),
                    meter.remaining_percent
                )
            })
            .collect::<Vec<_>>();
        (!meters.is_empty()).then(|| meters.join(", "))
    });
    projection
}

impl App {
    pub(super) fn sync_footer_runtime_projection(&mut self) {
        let Some(displayed_thread_id) = self.current_displayed_thread_id() else {
            self.chat_widget
                .set_footer_runtime_projection(FooterRuntimeProjection::default());
            return;
        };
        self.sync_footer_runtime_projection_for_thread(displayed_thread_id);
    }

    pub(super) fn sync_footer_runtime_projection_for_thread(
        &mut self,
        displayed_thread_id: codex_protocol::ThreadId,
    ) {
        let displayed_thread_id = displayed_thread_id.to_string();
        let runtime = self.account_runtime.as_ref().map(|(_, runtime)| runtime);
        let projection = project_footer_runtime(
            Some(&displayed_thread_id),
            runtime,
            self.account_slot_capability.as_ref(),
            &self.account_slots,
        );
        self.chat_widget.set_footer_runtime_projection(projection);
    }
}

#[cfg(test)]
#[path = "footer_projection_tests.rs"]
mod tests;
