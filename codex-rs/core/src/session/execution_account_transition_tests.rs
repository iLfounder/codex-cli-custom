use super::ExecutionAccountPreparationCancellation;
use std::sync::Arc;

#[test]
fn preparation_cancellation_has_one_exact_owner() {
    let cancellation = ExecutionAccountPreparationCancellation::default();
    let first = cancellation.begin().expect("first preparation");
    assert!(cancellation.begin().is_none());

    let unrelated = Arc::new(tokio_util::sync::CancellationToken::new());
    cancellation.clear(&unrelated);
    assert!(cancellation.begin().is_none());

    cancellation.cancel();
    assert!(first.is_cancelled());
    cancellation.clear(&first);
    assert!(cancellation.begin().is_some());
}
