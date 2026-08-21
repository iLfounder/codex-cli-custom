use super::*;

#[test]
fn unloading_target_conversion_uses_saturating_remaining_duration() {
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_secs();
    let target = Instant::now() + Duration::from_secs(60);

    let converted = unloading_target_unix(target);

    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_secs();
    assert!(converted >= i64::try_from(before.saturating_add(59)).unwrap_or(i64::MAX));
    assert!(converted <= i64::try_from(after.saturating_add(60)).unwrap_or(i64::MAX));
}
