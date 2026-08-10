use codex_protocol::workflow::WorkflowTaskStatus;
use codex_protocol::workflow::WorkflowUsage;
use serde_json::Value as JsonValue;

use super::ContextualUserFragment;

/// Completion notice for a background workflow run, rendered as a user-role
/// fragment so the owning thread sees it on its next turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkflowNotification {
    pub(crate) workflow_name: String,
    pub(crate) run_id: String,
    pub(crate) status: WorkflowTaskStatus,
    pub(crate) summary: String,
    pub(crate) failures: usize,
    pub(crate) error: Option<String>,
    pub(crate) usage: WorkflowUsage,
    pub(crate) result: Option<JsonValue>,
    pub(crate) result_available: bool,
    pub(crate) result_truncated: bool,
    pub(crate) result_preview: Option<String>,
    pub(crate) result_bytes: Option<u64>,
    pub(crate) next_offset: Option<u64>,
    pub(crate) result_error: Option<String>,
    pub(crate) next_action: Option<&'static str>,
}

impl ContextualUserFragment for WorkflowNotification {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<workflow_notification>", "</workflow_notification>")
    }

    fn body(&self) -> String {
        format!(
            "\n{}\n",
            serde_json::json!({
                "workflow_name": &self.workflow_name,
                "run_id": &self.run_id,
                "status": &self.status,
                "summary": &self.summary,
                "failures": self.failures,
                "error": &self.error,
                "usage": &self.usage,
                "result": &self.result,
                "result_available": self.result_available,
                "result_truncated": self.result_truncated,
                "result_preview": &self.result_preview,
                "result_bytes": self.result_bytes,
                "next_offset": self.next_offset,
                "result_error": &self.result_error,
                "next_action": self.next_action,
            })
        )
    }
}
