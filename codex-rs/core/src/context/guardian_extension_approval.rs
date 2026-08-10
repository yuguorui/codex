use super::ContextualUserFragment;

const TOOL_NAME_MAX_CHARS: usize = 128;

/// Bounded descriptor for an extension action reviewed through a host-held artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardianExtensionApproval {
    tool_name: String,
    sha256: String,
    byte_len: usize,
}

impl GuardianExtensionApproval {
    pub(crate) fn new(tool_name: &str, sha256: &str, byte_len: usize) -> Self {
        Self {
            tool_name: tool_name.chars().take(TOOL_NAME_MAX_CHARS).collect(),
            sha256: sha256.to_string(),
            byte_len,
        }
    }
}

impl ContextualUserFragment for GuardianExtensionApproval {
    fn role(&self) -> &'static str {
        "user"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<guardian_extension_approval>",
            "</guardian_extension_approval>",
        )
    }

    fn body(&self) -> String {
        format!(
            "\n{}\n",
            serde_json::json!({
                "toolName": &self.tool_name,
                "artifact": {
                    "sha256": &self.sha256,
                    "byteLength": self.byte_len,
                },
                "readerTool": "read_guardian_approval_artifact",
            })
        )
    }
}

#[cfg(test)]
#[path = "guardian_extension_approval_tests.rs"]
mod tests;
