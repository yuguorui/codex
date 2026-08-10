use codex_tools::ToolApprovalArtifact;
use std::sync::Arc;
use std::sync::Mutex;

pub(crate) const GUARDIAN_APPROVAL_ARTIFACT_PAGE_BYTES: usize = 8 * 1_024;

#[derive(Clone, Debug)]
pub(crate) struct GuardianApprovalArtifact {
    artifact: ToolApprovalArtifact,
    coverage: Arc<Mutex<ArtifactCoverage>>,
}

#[derive(Debug, Default)]
struct ArtifactCoverage {
    next_offset: usize,
    complete: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GuardianApprovalArtifactPage<'a> {
    pub(crate) sha256: &'a str,
    pub(crate) offset: usize,
    pub(crate) contents: &'a str,
    pub(crate) next_offset: Option<usize>,
}

impl GuardianApprovalArtifact {
    pub(crate) fn new(artifact: ToolApprovalArtifact) -> Self {
        Self {
            artifact,
            coverage: Arc::new(Mutex::new(ArtifactCoverage::default())),
        }
    }

    pub(crate) fn sha256(&self) -> &str {
        self.artifact.sha256()
    }

    pub(crate) fn byte_length(&self) -> usize {
        self.artifact.contents().len()
    }

    pub(crate) fn read_page(
        &self,
        sha256: &str,
        offset: usize,
    ) -> Result<GuardianApprovalArtifactPage<'_>, String> {
        if sha256 != self.sha256() {
            return Err("the requested artifact is not the action under review".to_string());
        }
        let mut coverage = self
            .coverage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if coverage.complete || offset != coverage.next_offset {
            return Err("offset must equal nextOffset from the preceding read".to_string());
        }

        let contents = self.artifact.contents();
        let mut end = offset
            .saturating_add(GUARDIAN_APPROVAL_ARTIFACT_PAGE_BYTES)
            .min(contents.len());
        while end > offset && !contents.is_char_boundary(end) {
            end -= 1;
        }
        if end == offset && offset < contents.len() {
            return Err("approval artifact page does not contain a UTF-8 boundary".to_string());
        }

        let next_offset = (end < contents.len()).then_some(end);
        coverage.next_offset = end;
        coverage.complete = next_offset.is_none();
        Ok(GuardianApprovalArtifactPage {
            sha256: self.sha256(),
            offset,
            contents: &contents[offset..end],
            next_offset,
        })
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.coverage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .complete
    }
}

impl PartialEq for GuardianApprovalArtifact {
    fn eq(&self, other: &Self) -> bool {
        self.artifact == other.artifact
    }
}

impl Eq for GuardianApprovalArtifact {}

#[cfg(test)]
#[path = "approval_artifact_tests.rs"]
mod tests;
