use codex_protocol::workflow::WorkflowTaskStatus;
use codex_protocol::workflow::WorkflowUsage;

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
    pub(crate) output_file: String,
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
                "output_file": &self.output_file,
            })
        )
    }
}
