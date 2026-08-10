use super::*;
use pretty_assertions::assert_eq;

#[test]
fn hard_text_bound_preserves_utf8_boundaries() {
    let value = "你好世界".repeat(100);

    let truncated = truncate_model_text(&value, /*max_bytes*/ 31);

    // A 31-byte budget leaves 17 bytes for the prefix, which floors to five whole
    // three-byte characters, so the result is those 15 bytes plus the 14-byte marker.
    assert_eq!(truncated.len(), 29);
    assert!(truncated.len() <= 31);
    assert!(truncated.ends_with("...[truncated]"));
    assert_eq!(&truncated[..15], "你好世界你");
}

#[test]
fn short_text_is_returned_unchanged() {
    assert_eq!(truncate_model_text("short", /*max_bytes*/ 31), "short");
}

#[test]
fn a_budget_below_the_marker_length_drops_the_marker() {
    // Eight bytes cannot hold a prefix and the fourteen-byte marker, so the caller
    // gets raw bytes instead of a claim about truncation.
    assert_eq!(
        truncate_model_text("script content", /*max_bytes*/ 8),
        "script c"
    );
}
