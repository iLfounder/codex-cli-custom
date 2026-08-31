use super::AccountId;
use pretty_assertions::assert_eq;

#[test]
fn account_id_accepts_only_canonical_codex_labels() {
    assert_eq!(AccountId::parse("C1").map(AccountId::number), Some(1));
    assert_eq!(AccountId::parse("C42").map(AccountId::number), Some(42));
    for rejected in ["C0", "C01", "CX1", "A1", "c1", "C", ""] {
        assert_eq!(AccountId::parse(rejected), None, "accepted {rejected}");
    }
}
