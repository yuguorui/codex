use crate::model_text::truncate_model_text;
use crate::service::WorkflowTaskSnapshot;
use codex_protocol::workflow::WorkflowTaskStatus;
use serde::Serialize;

const RECOVERY_ERROR_MAX_BYTES: usize = 512;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowRecoveryStatus {
    recovery_eligible: bool,
    reason: &'static str,
    may_require_reapproval: bool,
    identity_requirements: Vec<&'static str>,
    observed_restore_incompatibilities: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowRecoverySummary {
    recovery_eligible: bool,
    reason: &'static str,
    may_require_reapproval: bool,
    observed_restore_incompatibilities: Vec<&'static str>,
}

impl WorkflowRecoveryStatus {
    /// Summarizes the approved identity fields as a single requirement.
    ///
    /// A run that is not a recovery candidate carries no identity requirements, and
    /// inventing one would both grow the response and claim resume semantics for a
    /// run that cannot be resumed.
    pub(crate) fn compact_for_wait(&mut self) {
        if self.identity_requirements.is_empty() {
            return;
        }
        self.identity_requirements = vec!["sameApprovedWorkflowIdentity"];
    }

    /// Gives up the observed restore incompatibility list.
    ///
    /// The two wait ladders spend this at opposite ends, because the responses carry
    /// different amounts of run text. `bound_wait_workflows_output` spends it last:
    /// `WaitedWorkflowStatus` has no error or summary, so this list is the only
    /// diagnostic a batch entry has. `bound_wait_workflow_output` spends it first: a
    /// single-run response still shows that run's own error — the builder has already
    /// stubbed its summary by then — under a far tighter item cap, so the advisory list
    /// is the cheapest thing to lose there.
    ///
    /// Eligibility, reason, and reapproval stay intact so the model can still decide
    /// whether to resume. An empty list afterwards does not prove a clean resume, and
    /// the model cannot re-derive it from the text a response shows: the list is
    /// classified from up to `RECOVERY_ERROR_MAX_BYTES` of the run error, while wait and
    /// list responses only show the first 160 bytes of it. A per-run `WaitWorkflow`
    /// re-emits the list, so getting it back costs one wait per run.
    pub(crate) fn drop_observed_restore_incompatibilities(&mut self) {
        self.observed_restore_incompatibilities = Vec::new();
    }

    pub(crate) fn into_summary(self) -> WorkflowRecoverySummary {
        WorkflowRecoverySummary {
            recovery_eligible: self.recovery_eligible,
            reason: self.reason,
            may_require_reapproval: self.may_require_reapproval,
            observed_restore_incompatibilities: self.observed_restore_incompatibilities,
        }
    }
}

impl WorkflowRecoverySummary {
    /// Gives up the observed restore incompatibility list.
    ///
    /// Same trade-off as `WorkflowRecoveryStatus::drop_observed_restore_incompatibilities`,
    /// which documents it. This summary form only appears in batch wait entries, whose
    /// ladder spends it last.
    pub(crate) fn drop_observed_restore_incompatibilities(&mut self) {
        self.observed_restore_incompatibilities = Vec::new();
    }
}

pub(crate) fn workflow_recovery_status(snapshot: &WorkflowTaskSnapshot) -> WorkflowRecoveryStatus {
    let recovery_eligible = matches!(
        snapshot.status,
        WorkflowTaskStatus::Paused | WorkflowTaskStatus::Failed | WorkflowTaskStatus::Killed
    );
    let reason = match snapshot.status {
        WorkflowTaskStatus::Pending => "pending",
        WorkflowTaskStatus::Running => "running",
        WorkflowTaskStatus::Completed => "completed",
        WorkflowTaskStatus::Paused => "paused",
        WorkflowTaskStatus::Failed => "failed",
        WorkflowTaskStatus::Killed => "killed",
    };
    WorkflowRecoveryStatus {
        recovery_eligible,
        reason,
        may_require_reapproval: recovery_eligible,
        identity_requirements: if recovery_eligible {
            {
                vec![
                    "scriptSha256",
                    "args",
                    "childWorkflowDefinition",
                    "declaredInputs",
                    "executionIdentity",
                ]
            }
        } else {
            Default::default()
        },
        observed_restore_incompatibilities: if recovery_eligible {
            recovery_incompatibilities(snapshot)
        } else {
            Default::default()
        },
    }
}

fn recovery_incompatibilities(snapshot: &WorkflowTaskSnapshot) -> Vec<&'static str> {
    let Some(error) = snapshot.error.as_deref() else {
        return Vec::new();
    };
    let error = truncate_model_text(error, RECOVERY_ERROR_MAX_BYTES).to_ascii_lowercase();
    let mut incompatibilities = Vec::new();
    if error.contains("script content changed") {
        incompatibilities.push("scriptSha256");
    }
    if error.contains("workflow arguments") {
        incompatibilities.push("args");
    }
    if error.contains("declared inputs") {
        incompatibilities.push("declaredInputs");
    }
    if error.contains("execution context")
        || error.contains("execution identity")
        || error.contains("workspace and configuration")
    {
        incompatibilities.push("executionIdentity");
    }
    if error.contains("child workflow composition") {
        incompatibilities.push("childWorkflowDefinition");
    }
    incompatibilities
}

#[cfg(test)]
#[path = "workflow_recovery_tests.rs"]
mod tests;
