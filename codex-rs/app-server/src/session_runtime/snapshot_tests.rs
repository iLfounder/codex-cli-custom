use codex_protocol::protocol::ExecutionAccountBinding;
use pretty_assertions::assert_eq;

use codex_protocol::protocol::SessionSource;

use super::resolve_active_turn_binding;
use super::sanitize_runtime_source;
use super::sanitized_runtime_cwd;

#[test]
fn active_turn_binding_resolution_covers_store_outcomes() {
    let loaded = ExecutionAccountBinding {
        slot_id: "loaded-account".to_string(),
        generation: 7,
    };
    let durable = ExecutionAccountBinding {
        slot_id: "durable-account".to_string(),
        generation: 11,
    };

    assert_eq!(
        [
            resolve_active_turn_binding(Ok::<_, ()>(None), Some(&loaded)),
            resolve_active_turn_binding(Ok::<_, ()>(Some(durable.clone())), Some(&loaded)),
            resolve_active_turn_binding(
                Err::<Option<ExecutionAccountBinding>, _>(()),
                Some(&loaded),
            ),
        ],
        [Some(loaded), Some(durable), None]
    );
}

#[test]
fn runtime_identity_redacts_paths_and_custom_source_payloads() {
    assert_eq!(sanitized_runtime_cwd(), "<workspace>");
    assert_eq!(sanitize_runtime_source(&SessionSource::Cli), "cli");
    assert_eq!(
        sanitize_runtime_source(&SessionSource::Custom("private-workflow".to_string())),
        "custom"
    );
}
