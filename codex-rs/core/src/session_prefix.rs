use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::workflow::WorkflowCompletedEvent;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

use crate::context::ContextualUserFragment;
use crate::context::InterAgentCompletionMessage;
use crate::context::SubagentNotification;
use crate::context::WorkflowNotification;

const COMPLETION_MESSAGE_MAX_TOKENS: usize = 1_000;
const COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE: usize = 100;
const ERROR_MAX_TOKENS: usize =
    COMPLETION_MESSAGE_MAX_TOKENS - COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE;
const ERROR_NEXT_ACTION: &str = "This agent's turn failed. If you still need this agent, use the available collaboration tools to give it another task.";
const WORKFLOW_NOTIFICATION_MAX_TOKENS: usize = 1_000;
// Summary and error share a ~900-token payload budget so the rendered notice
// stays below WORKFLOW_NOTIFICATION_MAX_TOKENS once paths and JSON overhead
// are included.
const WORKFLOW_NOTIFICATION_SUMMARY_MAX_TOKENS: usize = 450;
const WORKFLOW_NOTIFICATION_ERROR_MAX_TOKENS: usize = 450;
const WORKFLOW_NOTIFICATION_PATH_MAX_BYTES: usize = 120;
const WORKFLOW_NOTIFICATION_NAME_MAX_BYTES: usize = 100;
const _: () = assert!(
    WORKFLOW_NOTIFICATION_SUMMARY_MAX_TOKENS + WORKFLOW_NOTIFICATION_ERROR_MAX_TOKENS
        < WORKFLOW_NOTIFICATION_MAX_TOKENS
);

// Helpers for model-visible session state markers that are stored in user-role
// messages but are not user intent.

// TODO(jif) unify with structured schema
pub(crate) fn format_subagent_notification_message(
    agent_reference: &str,
    status: &AgentStatus,
) -> String {
    SubagentNotification::new(agent_reference, status.clone()).render()
}

/// Renders a bounded completion notice for a background workflow run. The
/// summary and error fields are token-capped so the injected user-role message
/// stays below the manual-review threshold.
pub fn format_workflow_notification_message(event: &WorkflowCompletedEvent) -> String {
    let summary = truncate_text(
        &event.summary,
        TruncationPolicy::Tokens(WORKFLOW_NOTIFICATION_SUMMARY_MAX_TOKENS),
    );
    let error = event.error.as_deref().map(|error| {
        truncate_text(
            error,
            TruncationPolicy::Tokens(WORKFLOW_NOTIFICATION_ERROR_MAX_TOKENS),
        )
    });
    WorkflowNotification {
        workflow_name: truncate_text(
            &event.workflow_name,
            TruncationPolicy::Bytes(WORKFLOW_NOTIFICATION_NAME_MAX_BYTES),
        ),
        run_id: truncate_text(
            &event.run_id,
            TruncationPolicy::Bytes(WORKFLOW_NOTIFICATION_NAME_MAX_BYTES),
        ),
        status: event.status,
        summary,
        failures: event.failures.len(),
        error,
        usage: event.usage.clone(),
        output_file: truncate_text(
            &event.output_file.to_string_lossy(),
            TruncationPolicy::Bytes(WORKFLOW_NOTIFICATION_PATH_MAX_BYTES),
        ),
    }
    .render()
}

pub(crate) fn format_inter_agent_completion_message(
    task_name: AgentPath,
    sender: AgentPath,
    status: &AgentStatus,
) -> Option<String> {
    let payload = match status {
        AgentStatus::Completed(Some(message)) => message.clone(),
        AgentStatus::Completed(None) => String::new(),
        AgentStatus::Errored(error) => {
            let error = truncate_text(error, TruncationPolicy::Tokens(ERROR_MAX_TOKENS));
            format!("Agent errored: {error}\n\n{ERROR_NEXT_ACTION}")
        }
        AgentStatus::Shutdown => "Agent shut down.".to_string(),
        AgentStatus::NotFound => "Agent was not found.".to_string(),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => return None,
    };
    Some(InterAgentCompletionMessage::new(task_name, sender, payload).render())
}

#[cfg(test)]
#[path = "session_prefix_tests.rs"]
mod tests;

pub(crate) fn format_subagent_context_line(
    agent_reference: &str,
    agent_nickname: Option<&str>,
) -> String {
    match agent_nickname.filter(|nickname| !nickname.is_empty()) {
        Some(agent_nickname) => format!("- {agent_reference}: {agent_nickname}"),
        None => format!("- {agent_reference}"),
    }
}
