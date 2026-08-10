use codex_utils_output_truncation::approx_token_count;

use super::*;

#[test]
fn descriptor_is_a_separate_bounded_context_fragment() {
    let fragment = GuardianExtensionApproval::new(
        &"tool".repeat(1_000),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        usize::MAX,
    );

    assert!(fragment.requires_separate_message());
    assert!(approx_token_count(&fragment.render()) < 1_000);
    assert!(
        fragment
            .render()
            .contains("read_guardian_approval_artifact")
    );
}
