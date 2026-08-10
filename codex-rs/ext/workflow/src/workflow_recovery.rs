use crate::service::WorkflowTaskSnapshot;
use crate::workflow_result_tool::truncate_model_text;
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
    pub(crate) fn compact_for_wait(&mut self) {
        self.identity_requirements = vec!["sameApprovedWorkflowIdentity"];
    }

    /// Gives up the observed restore incompatibility list.
    ///
    /// Both wait ladders drop this before touching anything the model cannot
    /// recover elsewhere. Eligibility, reason, and reapproval stay intact so the
    /// model can still decide whether to resume, and the list is only derived from
    /// the run error, which `ListWorkflows` still reports.
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
    /// This is the only per-entry field a multi-run wait can shrink, and a batch of
    /// eight failed runs can carry five entries each, so the batch ladder needs it
    /// to stay inside the tool output cap without discarding the winner.
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
