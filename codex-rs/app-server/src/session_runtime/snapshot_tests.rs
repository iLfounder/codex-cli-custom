use codex_protocol::protocol::ExecutionAccountBinding;
use pretty_assertions::assert_eq;

use super::resolve_active_turn_binding;

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
