use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsageInfo;

use crate::AgentCompletion;
use crate::AgentRunError;
use crate::AgentRunProgress;

/// Identifies how a host orchestrator learned that an agent turn completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentCompletionSignal {
    /// The matching terminal turn event arrived normally.
    Event,
    /// The agent became idle before the matching terminal turn event arrived.
    TerminalStatus,
}

pub(super) fn ended_turn(status: Option<&AgentStatus>) -> bool {
    matches!(
        status,
        Some(
            AgentStatus::Interrupted
                | AgentStatus::Completed(_)
                | AgentStatus::Errored(_)
                | AgentStatus::Shutdown
                | AgentStatus::NotFound
        )
    )
}

pub(super) fn reconcile(
    status: Option<AgentStatus>,
    thread_id: ThreadId,
    token_usage: Option<&TokenUsageInfo>,
    progress: AgentRunProgress,
) -> Result<Option<AgentCompletion>, AgentRunError> {
    match status {
        Some(AgentStatus::Completed(output)) => Ok(Some(AgentCompletion {
            thread_id,
            output: output.unwrap_or_default(),
            token_usage: token_usage.cloned(),
            tool_uses: progress.tool_uses,
            signal: AgentCompletionSignal::TerminalStatus,
        })),
        Some(AgentStatus::Interrupted) => Err(AgentRunError::Codex {
            error: CodexErr::Interrupted,
            progress,
        }),
        Some(AgentStatus::Errored(message)) => Err(AgentRunError::Codex {
            error: CodexErr::Fatal(message),
            progress,
        }),
        Some(AgentStatus::Shutdown) => Err(AgentRunError::Codex {
            error: CodexErr::Fatal("agent shut down before completing".to_string()),
            progress,
        }),
        Some(AgentStatus::NotFound) => Err(AgentRunError::Codex {
            error: CodexErr::ThreadNotFound(thread_id),
            progress,
        }),
        Some(AgentStatus::PendingInit | AgentStatus::Running) | None => Ok(None),
    }
}
