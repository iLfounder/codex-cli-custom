//! Display-safe runtime values projected into the semantic footer.

/// Runtime-owned footer fields updated atomically by the app state owner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FooterRuntimeProjection {
    pub(crate) managed_slot_label: Option<String>,
    pub(crate) managed_slot_id: Option<String>,
    pub(crate) managed_slot_health: Option<String>,
    pub(crate) managed_slot_quota: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) session_name: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) thread_name: Option<String>,
    pub(crate) runtime_state: Option<String>,
    pub(crate) rotation_state: Option<String>,
}

/// ChatWidget-owned footer fields refreshed at existing model and usage commit seams.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FooterLiveContext {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) handle: Option<String>,
    pub(crate) context_usage: Option<String>,
}
