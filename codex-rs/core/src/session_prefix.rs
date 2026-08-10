use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::workflow::WorkflowCompletedEvent;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;
use serde_json::Value as JsonValue;

use crate::context::ContextualUserFragment;
use crate::context::InterAgentCompletionMessage;
use crate::context::WorkflowNotification;

const COMPLETION_MESSAGE_MAX_TOKENS: usize = 1_000;
const COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE: usize = 100;
const ERROR_MAX_TOKENS: usize =
    COMPLETION_MESSAGE_MAX_TOKENS - COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE;
const ERROR_NEXT_ACTION: &str = "This agent's turn failed. If you still need this agent, use the available collaboration tools to give it another task.";
const WORKFLOW_NOTIFICATION_MAX_TOKENS: usize = 1_000;
const WORKFLOW_NOTIFICATION_METADATA_MAX_BYTES: usize = 960;
/// Largest result prefix considered for direct owning-model delivery.
pub const WORKFLOW_NOTIFICATION_RESULT_CANDIDATE_MAX_BYTES: usize = 8 * 1_024;
const WORKFLOW_NOTIFICATION_RESULT_PREVIEW_MAX_BYTES: usize = 384;
const WORKFLOW_NOTIFICATION_TRUNCATION_MARKER: &str = "[truncated]";
const WORKFLOW_NOTIFICATION_NAME_MAX_CONTENT_BYTES: usize = 96;
const WORKFLOW_NOTIFICATION_RUN_ID_MAX_CONTENT_BYTES: usize = 120;
const WORKFLOW_RESULT_NEXT_ACTION: &str = "Call ReadWorkflowResult with runId set to this run_id and offset set to next_offset, then continue from each returned nextOffset until complete.";

/// Result data attached to an owning-model Workflow completion notification.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowNotificationResult {
    result: Option<JsonValue>,
    result_available: bool,
    result_truncated: bool,
    result_preview: Option<String>,
    result_bytes: Option<u64>,
    next_offset: Option<u64>,
    result_error: Option<String>,
}

impl WorkflowNotificationResult {
    /// Builds notification data from the leading chunk of a verified result artifact.
    pub fn from_chunk(chunk: &str, next_offset: u64, total_bytes: u64) -> serde_json::Result<Self> {
        let complete = next_offset == total_bytes;
        let result = complete.then(|| serde_json::from_str(chunk)).transpose()?;
        Ok(Self {
            result,
            result_available: true,
            result_truncated: !complete,
            result_preview: (!complete).then(|| {
                truncate_json_string(chunk, WORKFLOW_NOTIFICATION_RESULT_PREVIEW_MAX_BYTES)
            }),
            result_bytes: Some(total_bytes),
            next_offset: (!complete).then_some(0),
            result_error: None,
        })
    }

    /// Builds retry guidance when the verified result artifact could not be read.
    pub fn read_error(error: &str) -> Self {
        Self {
            result: None,
            result_available: false,
            result_truncated: false,
            result_preview: None,
            result_bytes: None,
            next_offset: Some(0),
            result_error: Some(truncate_json_string(
                error,
                WORKFLOW_NOTIFICATION_RESULT_PREVIEW_MAX_BYTES,
            )),
        }
    }

    fn route_through_result_tool(&mut self) {
        self.result = None;
        self.result_truncated = true;
        self.result_preview = None;
        self.next_offset = Some(0);
    }
}

// Helpers for model-visible session state markers that are stored in user-role
// messages but are not user intent.

// TODO(jif) unify with structured schema
/// Renders a bounded completion notice with a directly consumable terminal result.
pub fn format_workflow_notification_message(
    event: &WorkflowCompletedEvent,
    result: Option<WorkflowNotificationResult>,
) -> String {
    let result = result.unwrap_or(WorkflowNotificationResult {
        result: None,
        result_available: false,
        result_truncated: false,
        result_preview: None,
        result_bytes: None,
        next_offset: None,
        result_error: None,
    });
    let mut remaining = WORKFLOW_NOTIFICATION_METADATA_MAX_BYTES;

    let fields = [
        event.workflow_name.as_str(),
        event.run_id.as_str(),
        event.summary.as_str(),
        event.error.as_deref().unwrap_or_default(),
    ];
    let mut budgets = fields.map(minimum_json_content_budget);
    let minimum_total = budgets.iter().sum::<usize>();
    assert!(
        minimum_total <= remaining,
        "workflow notification markers must fit"
    );
    remaining -= minimum_total;

    for (index, maximum) in [
        (1, WORKFLOW_NOTIFICATION_RUN_ID_MAX_CONTENT_BYTES),
        (0, WORKFLOW_NOTIFICATION_NAME_MAX_CONTENT_BYTES),
    ] {
        let desired = capped_json_content_len(fields[index], maximum).min(maximum);
        let added = desired.saturating_sub(budgets[index]).min(remaining);
        budgets[index] += added;
        remaining -= added;
    }

    if event.error.is_some() {
        let error_index = 3;
        let summary_share = remaining.div_ceil(2);
        let summary_desired = capped_json_content_len(fields[2], budgets[2] + summary_share)
            .saturating_sub(budgets[2])
            .min(summary_share);
        budgets[2] += summary_desired;
        remaining -= summary_desired;

        let error_desired =
            capped_json_content_len(fields[error_index], budgets[error_index] + remaining)
                .saturating_sub(budgets[error_index])
                .min(remaining);
        budgets[error_index] += error_desired;
        remaining -= error_desired;

        let summary_desired = capped_json_content_len(fields[2], budgets[2] + remaining)
            .saturating_sub(budgets[2])
            .min(remaining);
        budgets[2] += summary_desired;
    } else {
        budgets[2] += capped_json_content_len(fields[2], budgets[2] + remaining)
            .saturating_sub(budgets[2])
            .min(remaining);
    }

    let render = |result: &WorkflowNotificationResult| {
        WorkflowNotification {
            workflow_name: truncate_json_string(fields[0], budgets[0]),
            run_id: truncate_json_string(fields[1], budgets[1]),
            status: event.status,
            summary: truncate_json_string(fields[2], budgets[2]),
            failures: event.failures.len(),
            error: event
                .error
                .as_ref()
                .map(|_| truncate_json_string(fields[3], budgets[3])),
            usage: event.usage.clone(),
            result: result.result.clone(),
            result_available: result.result_available,
            result_truncated: result.result_truncated,
            result_preview: result.result_preview.clone(),
            result_bytes: result.result_bytes,
            next_offset: result.next_offset,
            result_error: result.result_error.clone(),
            next_action: (result.result_truncated || result.result_error.is_some())
                .then_some(WORKFLOW_RESULT_NEXT_ACTION),
        }
        .render()
    };
    let mut result = result;
    let mut message = render(&result);
    if approx_token_count(&message) >= WORKFLOW_NOTIFICATION_MAX_TOKENS && result.result.is_some() {
        result.route_through_result_tool();
        message = render(&result);
    }
    assert!(approx_token_count(&message) < WORKFLOW_NOTIFICATION_MAX_TOKENS);
    message
}

fn minimum_json_content_budget(value: &str) -> usize {
    capped_json_content_len(value, WORKFLOW_NOTIFICATION_TRUNCATION_MARKER.len())
        .min(WORKFLOW_NOTIFICATION_TRUNCATION_MARKER.len())
}

fn capped_json_content_len(value: &str, maximum: usize) -> usize {
    let mut bytes = 0_usize;
    for character in value.chars() {
        bytes = bytes.saturating_add(json_escaped_char_len(character));
        if bytes > maximum {
            return maximum.saturating_add(1);
        }
    }
    bytes
}

fn truncate_json_string(value: &str, maximum: usize) -> String {
    if capped_json_content_len(value, maximum) <= maximum {
        return value.to_string();
    }

    let content_budget = maximum.saturating_sub(WORKFLOW_NOTIFICATION_TRUNCATION_MARKER.len());
    let mut result = String::new();
    let mut used = 0_usize;
    for character in value.chars() {
        let character_bytes = json_escaped_char_len(character);
        if character_bytes > content_budget.saturating_sub(used) {
            break;
        }
        result.push(character);
        used += character_bytes;
    }
    result.push_str(WORKFLOW_NOTIFICATION_TRUNCATION_MARKER);
    result
}

fn json_escaped_char_len(character: char) -> usize {
    match character {
        '\"' | '\\' | '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r' => 2,
        '\u{0000}'..='\u{001f}' => 6,
        character => character.len_utf8(),
    }
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
