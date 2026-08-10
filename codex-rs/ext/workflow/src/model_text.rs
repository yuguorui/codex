//! Byte-bounded text helpers shared by every model-facing workflow response.
//!
//! Leaf module: it depends on nothing inside this crate, so response builders can
//! share one truncation rule without forming import cycles.

/// Marker appended to text that a byte budget cut short.
///
/// Public so callers can size a stub budget that still shows the marker; a budget
/// below `TRUNCATION_MARKER.len()` emits a bare prefix instead.
pub(crate) const TRUNCATION_MARKER: &str = "...[truncated]";

/// Truncates text to `max_bytes`, marking the cut and never splitting a UTF-8 char.
///
/// When the budget cannot hold both a prefix and the marker, the marker is dropped
/// rather than emitting a response that claims more precision than it has.
pub(crate) fn truncate_model_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    if max_bytes <= TRUNCATION_MARKER.len() {
        return value[..floor_char_boundary(value, max_bytes)].to_string();
    }
    let prefix_end = floor_char_boundary(value, max_bytes - TRUNCATION_MARKER.len());
    let prefix = &value[..prefix_end];
    format!("{prefix}{TRUNCATION_MARKER}")
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
#[path = "model_text_tests.rs"]
mod tests;
