use super::revision_meets_lower_bound;
use pretty_assertions::assert_eq;

#[test]
fn authoritative_revision_may_match_or_advance_but_not_regress() {
    assert_eq!(
        (
            revision_meets_lower_bound(10, 10),
            revision_meets_lower_bound(12, 10),
            revision_meets_lower_bound(9, 10),
        ),
        (true, true, false)
    );
}
