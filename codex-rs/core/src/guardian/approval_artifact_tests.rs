use codex_tools::ToolApprovalArtifact;
use pretty_assertions::assert_eq;

use super::GUARDIAN_APPROVAL_ARTIFACT_PAGE_BYTES;
use super::GuardianApprovalArtifact;

#[test]
fn exact_hash_pages_must_be_read_contiguously_to_complete_coverage() {
    let contents = format!(
        "{}{}",
        "a".repeat(GUARDIAN_APPROVAL_ARTIFACT_PAGE_BYTES - 1),
        "界".repeat(400)
    );
    let artifact =
        GuardianApprovalArtifact::new(ToolApprovalArtifact::from_contents(contents.clone()));
    let sha256 = artifact.sha256().to_string();

    assert!(artifact.read_page("0", 0).is_err());
    assert!(artifact.read_page(&sha256, 1).is_err());
    assert!(!artifact.is_complete());

    let mut reconstructed = String::new();
    let mut offset = 0;
    loop {
        let page = artifact
            .read_page(&sha256, offset)
            .expect("next contiguous page should be readable");
        assert!(page.contents.len() <= GUARDIAN_APPROVAL_ARTIFACT_PAGE_BYTES);
        reconstructed.push_str(page.contents);
        let Some(next_offset) = page.next_offset else {
            break;
        };
        assert!(artifact.read_page(&sha256, offset).is_err());
        offset = next_offset;
    }

    assert_eq!(reconstructed, contents);
    assert!(artifact.is_complete());
    assert!(artifact.read_page(&sha256, offset).is_err());
}
