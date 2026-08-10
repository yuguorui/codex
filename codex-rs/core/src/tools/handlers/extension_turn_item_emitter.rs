use std::sync::Arc;
use std::sync::Weak;

use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_tools::ExtensionTurnItem;
use codex_tools::ToolApprovalDecision;
use codex_tools::ToolApprovalFuture;
use codex_tools::ToolApprovalOutcome;
use codex_tools::ToolApprovalOutcomeFuture;
use codex_tools::ToolApprovalRequest;
use codex_tools::ToolApprovalReviewMode;
use codex_tools::ToolApprovalReviewRequest;
use codex_tools::ToolExecutionEnvironment;
use codex_tools::ToolName;
use codex_tools::ToolTokenBudget;
use codex_tools::TurnActivitySubscription;
use codex_tools::TurnItemEmissionFuture;
use codex_tools::TurnItemEmitter;

use super::extension_turn_activity::CoreTurnActivitySubscription;
use crate::guardian::GuardianReviewContext;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::approvals::ApprovalAction;
use crate::tools::approvals::ApprovalContext;
use crate::tools::hook_names::HookToolName;

pub(super) struct CoreTurnItemEmitter {
    session: Weak<Session>,
    turn: Weak<TurnContext>,
    call_id: String,
    tool_name: ToolName,
    permission_hook_name: HookToolName,
    approval_review_mode: ToolApprovalReviewMode,
    turn_activity: Arc<CoreTurnActivitySubscription>,
    execution_environments: Vec<ToolExecutionEnvironment>,
}

impl CoreTurnItemEmitter {
    pub(super) fn new(
        session: Weak<Session>,
        turn: Weak<TurnContext>,
        call_id: String,
        tool_name: ToolName,
        approval_review_mode: ToolApprovalReviewMode,
        turn_activity: Arc<CoreTurnActivitySubscription>,
        execution_environments: Vec<ToolExecutionEnvironment>,
    ) -> Self {
        let permission_hook_name = HookToolName::new(tool_name.name.clone());
        Self {
            session,
            turn,
            call_id,
            tool_name,
            permission_hook_name,
            approval_review_mode,
            turn_activity,
            execution_environments,
        }
    }

    async fn request_approval_outcome(
        &self,
        request: ToolApprovalReviewRequest,
    ) -> ToolApprovalOutcome {
        if request.prompt.call_id != self.call_id {
            return ToolApprovalOutcome::Denied {
                rejection: "extension approval call id did not match the active tool call"
                    .to_string(),
                source: codex_tools::ToolApprovalDenialSource::Configuration,
            };
        }
        let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
            return ToolApprovalOutcome::Unavailable;
        };
        let action = ApprovalAction::ExtensionTool {
            id: self.call_id.clone(),
            tool_name: self.permission_hook_name.name().to_string(),
            hook_tool_name: self.permission_hook_name.clone(),
            prompt: request.prompt,
            action: request.action,
            artifact: request.artifact,
        };
        let approval_context = ApprovalContext {
            review_context: GuardianReviewContext::from(turn),
            cancellation_token: None,
            call_id: self.call_id.clone(),
            tool_name: self.tool_name.clone(),
            strict_auto_review: self.approval_review_mode
                == ToolApprovalReviewMode::StrictAutomatic,
            approval_reason: None,
            retry_reason: None,
            network_approval_context: None,
        };
        session
            .request_extension_tool_approval(action, approval_context)
            .await
    }

    async fn request_user_approval_outcome(
        &self,
        request: ToolApprovalRequest,
    ) -> ToolApprovalOutcome {
        if request.call_id != self.call_id {
            return ToolApprovalOutcome::Denied {
                rejection: "extension approval call id did not match the active tool call"
                    .to_string(),
                source: codex_tools::ToolApprovalDenialSource::Configuration,
            };
        }
        let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
            return ToolApprovalOutcome::Unavailable;
        };
        let approval_context = ApprovalContext {
            review_context: GuardianReviewContext::from(turn),
            cancellation_token: None,
            call_id: self.call_id.clone(),
            tool_name: self.tool_name.clone(),
            strict_auto_review: false,
            approval_reason: None,
            retry_reason: None,
            network_approval_context: None,
        };
        session
            .request_extension_tool_user_approval(request, approval_context)
            .await
    }
}

struct CoreToolTokenBudget {
    session: Weak<Session>,
}

impl ToolTokenBudget for CoreToolTokenBudget {
    fn total(&self) -> u64 {
        self.snapshot().map_or(0, |(total, _)| total)
    }

    fn spent(&self) -> u64 {
        self.snapshot().map_or(0, |(_, spent)| spent)
    }
}

impl CoreToolTokenBudget {
    fn snapshot(&self) -> Option<(u64, u64)> {
        self.session
            .upgrade()?
            .services
            .agent_control
            .rollout_budget()
            .token_snapshot()
    }
}

async fn emit_legacy_events(session: &Session, turn: &TurnContext, legacy_events: Vec<EventMsg>) {
    for msg in legacy_events {
        session
            .send_event_raw(Event {
                id: turn.sub_id.clone(),
                msg,
            })
            .await;
    }
}

impl TurnItemEmitter for CoreTurnItemEmitter {
    fn emit_started<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let ExtensionTurnItem {
                item,
                legacy_events,
            } = item;
            let item = TurnItem::Extension(item);
            session.emit_turn_item_started(turn.as_ref(), &item).await;
            emit_legacy_events(session.as_ref(), turn.as_ref(), legacy_events).await;
        })
    }

    fn emit_completed<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let ExtensionTurnItem {
                item,
                legacy_events,
            } = item;
            let item = TurnItem::Extension(item);
            session.emit_turn_item_completed(turn.as_ref(), item).await;
            emit_legacy_events(session.as_ref(), turn.as_ref(), legacy_events).await;
        })
    }

    fn request_approval<'a>(&'a self, request: ToolApprovalRequest) -> ToolApprovalFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return ToolApprovalDecision::Unavailable;
            };
            let approve_label = request.approve_label.clone();
            let response = session
                .request_user_input(
                    turn.as_ref(),
                    request.call_id,
                    codex_protocol::request_user_input::RequestUserInputArgs {
                        questions: vec![
                            codex_protocol::request_user_input::RequestUserInputQuestion {
                                id: request.id.clone(),
                                header: request.header,
                                question: request.question,
                                is_other: false,
                                is_secret: false,
                                options: Some(vec![
                                    codex_protocol::request_user_input::RequestUserInputQuestionOption {
                                        label: request.approve_label,
                                        description: "Approve this extension action.".to_string(),
                                    },
                                    codex_protocol::request_user_input::RequestUserInputQuestionOption {
                                        label: request.deny_label,
                                        description: "Do not perform this extension action."
                                            .to_string(),
                                    },
                                ]),
                            },
                        ],
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await;
            let approved = response
                .as_ref()
                .and_then(|response| response.answers.get(&request.id))
                .is_some_and(|answer| answer.answers.iter().any(|value| value == &approve_label));
            if approved {
                ToolApprovalDecision::Approved
            } else {
                ToolApprovalDecision::Denied
            }
        })
    }

    fn request_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalReviewRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        Box::pin(self.request_approval_outcome(request))
    }

    fn request_user_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        Box::pin(self.request_user_approval_outcome(request))
    }

    fn approval_review_mode(&self) -> ToolApprovalReviewMode {
        self.approval_review_mode
    }

    fn token_budget(&self) -> Option<Arc<dyn ToolTokenBudget>> {
        let budget = CoreToolTokenBudget {
            session: self.session.clone(),
        };
        budget
            .snapshot()
            .map(|_| Arc::new(budget) as Arc<dyn ToolTokenBudget>)
    }

    fn turn_activity(&self) -> Option<Arc<dyn TurnActivitySubscription>> {
        Some(self.turn_activity.clone())
    }

    fn execution_environments(&self) -> Vec<ToolExecutionEnvironment> {
        self.execution_environments.clone()
    }
}

#[cfg(test)]
#[path = "extension_turn_item_emitter_tests.rs"]
mod tests;
