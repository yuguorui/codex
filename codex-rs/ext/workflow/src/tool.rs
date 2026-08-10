use codex_agent_extension::AgentRunner;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolApprovalDecision;
use codex_extension_api::ToolApprovalRequest;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AskForApproval;
use codex_tools::ToolExposure;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Weak;

use crate::discovery::WorkflowInput;
use crate::discovery::active_plugin_workflow_roots;
use crate::discovery::resolve_workflow;
use crate::service::WorkflowLaunchRequest;
use crate::service::WorkflowService;
use crate::spec::RUN_WORKFLOW_TOOL_ALIAS;
use crate::spec::WORKFLOW_TOOL_NAME;
use crate::spec::workflow_tool_spec;

const APPROVAL_PREVIEW_SEGMENT_BYTES: usize = 2_000;

#[derive(Clone, Copy)]
enum WorkflowToolKind {
    Canonical,
    CompatibilityAlias,
}

impl WorkflowToolKind {
    fn name(self) -> &'static str {
        match self {
            Self::Canonical => WORKFLOW_TOOL_NAME,
            Self::CompatibilityAlias => RUN_WORKFLOW_TOOL_ALIAS,
        }
    }
}

pub(crate) struct WorkflowToolExecutor {
    thread_id: ThreadId,
    config: Config,
    service: WorkflowService,
    agent_runner: AgentRunner,
    thread_manager: Weak<ThreadManager>,
    kind: WorkflowToolKind,
}

impl WorkflowToolExecutor {
    pub(crate) fn new(
        thread_id: ThreadId,
        config: Config,
        service: WorkflowService,
        agent_runner: AgentRunner,
        thread_manager: Weak<ThreadManager>,
    ) -> Self {
        Self {
            thread_id,
            config,
            service,
            agent_runner,
            thread_manager,
            kind: WorkflowToolKind::Canonical,
        }
    }

    pub(crate) fn compatibility_alias(
        thread_id: ThreadId,
        config: Config,
        service: WorkflowService,
        agent_runner: AgentRunner,
        thread_manager: Weak<ThreadManager>,
    ) -> Self {
        Self {
            thread_id,
            config,
            service,
            agent_runner,
            thread_manager,
            kind: WorkflowToolKind::CompatibilityAlias,
        }
    }
}

impl ToolExecutor<ToolCall> for WorkflowToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.kind.name())
    }

    fn spec(&self) -> ToolSpec {
        workflow_tool_spec(self.kind.name())
    }

    fn exposure(&self) -> ToolExposure {
        match self.kind {
            WorkflowToolKind::Canonical => ToolExposure::Direct,
            WorkflowToolKind::CompatibilityAlias => ToolExposure::Hidden,
        }
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let input = serde_json::from_str::<WorkflowInput>(invocation.function_arguments()?)
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!("invalid Workflow input: {error}"))
                })?;
            let plugin_roots =
                active_plugin_workflow_roots(&self.thread_manager, &self.config).await;
            let resolved = resolve_workflow(
                input,
                &self.config.cwd,
                &self.config.codex_home,
                &plugin_roots,
            )
            .await
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            if self.config.permissions.approval_policy.value() != AskForApproval::Never {
                let meta = &resolved.script.meta;
                let title = meta.title.as_deref().unwrap_or(&meta.name);
                let phases = if meta.phases.is_empty() {
                    "(none)".to_string()
                } else {
                    bounded_text(
                        &meta
                            .phases
                            .iter()
                            .map(|phase| phase.title.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        1_000,
                    )
                };
                let script_hash =
                    format!("{:x}", Sha256::digest(resolved.script.source.as_bytes()));
                let preview = approval_script_preview(&resolved.script.source);
                let source = resolved.origin.approval_label();
                let shadow_notice = if resolved.shadows_existing {
                    "\nWarning: this file shadows a lower-priority workflow with the same name."
                } else {
                    ""
                };
                let decision = invocation
                    .turn_item_emitter
                    .request_approval(ToolApprovalRequest {
                        call_id: invocation.call_id.clone(),
                        id: format!("workflow_approval_{}", invocation.call_id),
                        header: "Workflow".to_string(),
                        question: format!(
                            "Review dynamic workflow before running\n\nName: {}\nTitle: {}\nDescription: {}\nPhases: {phases}\nSource: {source}{shadow_notice}\nScript size: {} bytes\nSHA-256: {script_hash}\n\n{preview}",
                            bounded_text(&meta.name, 200),
                            bounded_text(title, 200),
                            bounded_text(&meta.description, 500),
                            resolved.script.source.len(),
                        ),
                        approve_label: "Run workflow".to_string(),
                        deny_label: "Cancel".to_string(),
                    })
                    .await;
                match decision {
                    ToolApprovalDecision::Approved => {}
                    ToolApprovalDecision::Denied => {
                        return Err(FunctionCallError::RespondToModel(
                            "dynamic workflow was not approved".to_string(),
                        ));
                    }
                    ToolApprovalDecision::Unavailable => {
                        return Err(FunctionCallError::RespondToModel(
                            "dynamic workflow approval is required but unavailable in this client"
                                .to_string(),
                        ));
                    }
                }
            }
            let launch = self
                .service
                .launch(WorkflowLaunchRequest {
                    thread_id: self.thread_id,
                    turn_id: invocation.turn_id,
                    config: self.config.clone(),
                    resolved,
                    agent_runner: self.agent_runner.clone(),
                    token_budget: invocation.turn_item_emitter.token_budget(),
                    plugin_roots,
                })
                .await
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let value = serde_json::to_value(launch).map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "failed to serialize Workflow result: {error}"
                ))
            })?;
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        bounded.push_str("...");
    }
    bounded
}

fn approval_script_preview(source: &str) -> String {
    let full_preview_bytes = APPROVAL_PREVIEW_SEGMENT_BYTES.saturating_mul(2);
    if source.len() <= full_preview_bytes {
        return format!("Script (complete):\n{source}");
    }

    let head_end = floor_char_boundary(source, APPROVAL_PREVIEW_SEGMENT_BYTES);
    let tail_start = ceil_char_boundary(
        source,
        source.len().saturating_sub(APPROVAL_PREVIEW_SEGMENT_BYTES),
    );
    let omitted = tail_start.saturating_sub(head_end);
    format!(
        "Script preview (INCOMPLETE: {omitted} bytes omitted; verify the SHA-256 before approving):\n--- first {head_end} bytes ---\n{}\n--- omitted {omitted} bytes ---\n--- last {} bytes ---\n{}",
        &source[..head_end],
        source.len().saturating_sub(tail_start),
        &source[tail_start..],
    )
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
