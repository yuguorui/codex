use std::sync::Arc;

use codex_tools::ConversationHistory;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolApprovalReviewMode;
use codex_tools::ToolCall as ExtensionToolCall;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use codex_utils_string::to_ascii_json_string;

use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::extension_agent_configuration::project_agent_configuration;
use crate::tools::handlers::extension_environment::project_execution_environments;
use crate::tools::handlers::extension_turn_activity::CoreTurnActivitySubscription;
use crate::tools::handlers::extension_turn_item_emitter::CoreTurnItemEmitter;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::turn_metadata::McpTurnMetadataContext;

pub(crate) struct ExtensionToolAdapter(Arc<dyn codex_tools::ToolExecutor<ExtensionToolCall>>);

impl ExtensionToolAdapter {
    pub(crate) fn new(executor: Arc<dyn codex_tools::ToolExecutor<ExtensionToolCall>>) -> Self {
        Self(executor)
    }
}

impl ToolExecutor<ToolInvocation> for ExtensionToolAdapter {
    fn tool_name(&self) -> ToolName {
        self.0.tool_name()
    }

    fn spec(&self) -> ToolSpec {
        self.0.spec()
    }

    fn exposure(&self) -> crate::tools::registry::ToolExposure {
        self.0.exposure()
    }

    fn availability(&self) -> codex_tools::ToolAvailability {
        self.0.availability()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.0.supports_parallel_tool_calls()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.0.search_info()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move { self.0.handle(to_extension_call(&invocation).await).await })
    }
}

impl CoreToolRuntime for ExtensionToolAdapter {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        match payload {
            ToolPayload::Function { .. } => true,
            ToolPayload::Custom { .. } => match self.0.spec() {
                ToolSpec::Freeform(_) => true,
                ToolSpec::Namespace(namespace) => namespace.tools.iter().any(|tool| {
                    matches!(
                        tool,
                        ResponsesApiNamespaceTool::Custom(tool)
                            if tool.name == self.0.tool_name().name
                    )
                }),
                ToolSpec::Function(_)
                | ToolSpec::ToolSearch { .. }
                | ToolSpec::WebSearch { .. } => false,
            },
            ToolPayload::ToolSearch { .. } => false,
        }
    }
}

async fn to_extension_call(invocation: &ToolInvocation) -> ExtensionToolCall {
    let conversation_history =
        ConversationHistory::new(invocation.session.clone_history().await.into_raw_items());
    let codex_turn_metadata = invocation
        .turn
        .turn_metadata_state
        .current_meta_value_for_mcp_request(McpTurnMetadataContext {
            model: invocation.step_context.model_info.slug.as_str(),
            reasoning_effort: invocation.step_context.reasoning_effort.clone(),
        })
        .and_then(|metadata| to_ascii_json_string(&metadata).ok());
    let turn_state = invocation
        .session
        .input_queue
        .turn_state_for_sub_id(&invocation.session.active_turn, &invocation.turn.sub_id)
        .await;
    let (activity_rx, pending_activity) = invocation
        .session
        .input_queue
        .subscribe_activity(turn_state.as_deref())
        .await;
    let strict_auto_review = if let Some(turn_state) = &turn_state {
        turn_state.lock().await.strict_auto_review_enabled()
    } else {
        false
    };
    let approval_review_mode = if strict_auto_review {
        ToolApprovalReviewMode::StrictAutomatic
    } else {
        match invocation.step_context.approvals_reviewer {
            codex_protocol::config_types::ApprovalsReviewer::User => ToolApprovalReviewMode::User,
            codex_protocol::config_types::ApprovalsReviewer::AutoReview => {
                ToolApprovalReviewMode::Automatic
            }
        }
    };
    let turn_activity = Arc::new(CoreTurnActivitySubscription::new(
        activity_rx,
        pending_activity,
        turn_state,
    ));
    let (environments, execution_environments) = project_execution_environments(invocation).await;
    let agent_configuration = project_agent_configuration(invocation).await;
    ExtensionToolCall {
        turn_id: invocation.turn.sub_id.clone(),
        call_id: invocation.call_id.clone(),
        tool_name: invocation.tool_name.clone(),
        model: invocation.step_context.model_info.slug.clone(),
        codex_turn_metadata,
        truncation_policy: invocation.step_context.model_info.truncation_policy.into(),
        conversation_history,
        turn_item_emitter: Arc::new(CoreTurnItemEmitter::new(
            Arc::downgrade(&invocation.session),
            Arc::downgrade(&invocation.turn),
            invocation.call_id.clone(),
            invocation.tool_name.clone(),
            approval_review_mode,
            turn_activity,
            execution_environments,
        )),
        environments,
        agent_configuration: Some(agent_configuration),
        payload: invocation.payload.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_extension_items::ExtensionItem;
    use codex_extension_items::web_search::WebSearchItem;
    use codex_protocol::config_types::ApprovalsReviewer;
    use codex_protocol::config_types::ReasoningSummary;
    use codex_protocol::items::TurnItem;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::EventMsg;
    use codex_tools::ExtensionTurnItem;
    use core_test_support::responses::strip_response_item_id;
    use core_test_support::responses::strip_response_item_ids;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::ExtensionToolAdapter;
    use crate::config::Config;
    use crate::session::step_context::StepContext;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::hook_names::HookToolName;
    use crate::tools::registry::CoreToolRuntime;
    use crate::tools::registry::PostToolUsePayload;
    use crate::tools::registry::PreToolUsePayload;
    use crate::turn_diff_tracker::TurnDiffTracker;

    struct StubExtensionExecutor;

    impl codex_extension_api::ToolExecutor<codex_tools::ToolCall> for StubExtensionExecutor {
        fn tool_name(&self) -> codex_tools::ToolName {
            codex_tools::ToolName::plain("extension_echo")
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: "extension_echo".to_string(),
                description: "Echoes arguments.".to_string(),
                strict: true,
                parameters: codex_tools::parse_tool_input_schema(&json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                    },
                    "required": ["message"],
                    "additionalProperties": false,
                }))
                .expect("extension schema should parse"),
                output_schema: None,
                defer_loading: None,
            })
        }

        fn handle(&self, _call: codex_tools::ToolCall) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async {
                Ok(
                    Box::new(codex_tools::JsonToolOutput::new(json!({ "ok": true })))
                        as Box<dyn codex_tools::ToolOutput>,
                )
            })
        }
    }

    struct CapturingExtensionExecutor {
        captured_call: Arc<Mutex<Option<codex_tools::ToolCall>>>,
    }

    impl codex_extension_api::ToolExecutor<codex_tools::ToolCall> for CapturingExtensionExecutor {
        fn tool_name(&self) -> codex_tools::ToolName {
            codex_tools::ToolName::plain("extension_echo")
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: "extension_echo".to_string(),
                description: "Captures arguments.".to_string(),
                strict: false,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
                defer_loading: None,
            })
        }

        fn handle(&self, call: codex_tools::ToolCall) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(self.handle_call(call))
        }
    }

    impl CapturingExtensionExecutor {
        async fn handle_call(
            &self,
            call: codex_tools::ToolCall,
        ) -> Result<Box<dyn codex_tools::ToolOutput>, codex_tools::FunctionCallError> {
            call.turn_item_emitter
                .emit_started(ExtensionTurnItem {
                    item: ExtensionItem::WebSearch(WebSearchItem {
                        id: call.call_id.clone(),
                        query: String::new(),
                        action: None,
                        results: None,
                    }),
                    legacy_events: Vec::new(),
                })
                .await;
            *self.captured_call.lock().await = Some(call);
            Ok(
                Box::new(codex_tools::JsonToolOutput::new(json!({ "ok": true })))
                    as Box<dyn codex_tools::ToolOutput>,
            )
        }
    }

    #[test]
    fn function_extensions_reject_custom_payloads() {
        let handler = ExtensionToolAdapter::new(Arc::new(StubExtensionExecutor));

        assert!(handler.matches_kind(&ToolPayload::Function {
            arguments: "{}".to_string(),
        }));
        assert!(!handler.matches_kind(&ToolPayload::Custom {
            input: "raw input".to_string(),
        }));
    }

    #[tokio::test]
    async fn exposes_generic_hook_payloads() {
        let handler = ExtensionToolAdapter::new(Arc::new(StubExtensionExecutor));
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let turn = Arc::new(turn);
        let invocation = ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "call-extension".to_string(),
            tool_name: codex_tools::ToolName::plain("extension_echo"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({ "message": "hello" }).to_string(),
            },
        };
        let output = codex_tools::JsonToolOutput::new(json!({ "ok": true }));

        assert_eq!(
            CoreToolRuntime::pre_tool_use_payload(&handler, &invocation),
            Some(PreToolUsePayload {
                tool_name: HookToolName::new("extension_echo"),
                tool_input: json!({ "message": "hello" }),
            })
        );
        assert_eq!(
            CoreToolRuntime::post_tool_use_payload(&handler, &invocation, &output),
            Some(PostToolUsePayload {
                tool_name: HookToolName::new("extension_echo"),
                tool_use_id: "call-extension".to_string(),
                tool_input: json!({ "message": "hello" }),
                tool_response: json!({ "ok": true }),
            })
        );
    }

    #[tokio::test]
    async fn passes_turn_fields_and_scoped_turn_item_emitter_to_extension_call() {
        let captured_call = Arc::new(Mutex::new(None));
        let handler = ExtensionToolAdapter::new(Arc::new(CapturingExtensionExecutor {
            captured_call: Arc::clone(&captured_call),
        }));
        let (session, turn, rx) = crate::session::tests::make_session_and_context_with_rx().await;
        let weak_session = Arc::downgrade(&session);
        let weak_turn = Arc::downgrade(&turn);
        let turn_id = turn.sub_id.clone();
        let expected_sandbox_cwds = turn
            .environments
            .turn_environments()
            .map(|environment| Some(environment.cwd().clone()))
            .collect::<Vec<_>>();
        let history_item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "extension history".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        session
            .record_conversation_items(&turn, std::slice::from_ref(&history_item))
            .await;
        let expected_history_item = strip_response_item_id(
            session
                .clone_history()
                .await
                .raw_items()
                .next()
                .expect("history item")
                .clone(),
        );
        let raw_history_event = rx.recv().await.expect("history raw response item event");
        let EventMsg::RawResponseItem(raw_history_item) = raw_history_event.msg else {
            panic!("expected raw response item event");
        };
        assert_eq!(
            strip_response_item_id(raw_history_item.item),
            expected_history_item
        );
        let expected_base_instructions = session.get_base_instructions().await;
        let mut step_context = StepContext::for_test(Arc::clone(&turn));
        let step = Arc::get_mut(&mut step_context).expect("unshared test step context");
        let mut model_info = (*step.model_info).clone();
        model_info.slug = "effective-step-model".to_string();
        step.model_info = Arc::new(model_info);
        step.reasoning_effort = Some(ReasoningEffort::High);
        step.reasoning_summary = ReasoningSummary::Detailed;
        step.service_tier = Some("priority".to_string());
        step.approval_policy = AskForApproval::Never;
        step.approvals_reviewer = ApprovalsReviewer::AutoReview;
        let expected_model = step.model_info.slug.clone();
        let expected_truncation_policy = step.model_info.truncation_policy.into();
        let mut expected_agent_config = (*turn.config).clone();
        expected_agent_config.model = Some(expected_model.clone());
        expected_agent_config.model_reasoning_effort = Some(ReasoningEffort::High);
        expected_agent_config.model_reasoning_summary = Some(ReasoningSummary::Detailed);
        expected_agent_config.service_tier = Some("priority".to_string());
        expected_agent_config.permissions.approval_policy =
            codex_config::Constrained::allow_only(AskForApproval::Never);
        expected_agent_config.approvals_reviewer = ApprovalsReviewer::AutoReview;
        expected_agent_config.base_instructions = Some(expected_base_instructions.text);
        expected_agent_config.base_instructions_provenance = expected_base_instructions.provenance;
        expected_agent_config
            .developer_instructions
            .clone_from(&turn.developer_instructions);
        expected_agent_config.personality = turn.personality;
        expected_agent_config.model_provider = turn.provider.info().clone();
        let invocation = ToolInvocation {
            session,
            step_context,
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "call-extension".to_string(),
            tool_name: codex_tools::ToolName::plain("extension_echo"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({ "message": "hello" }).to_string(),
            },
        };

        crate::tools::registry::ToolExecutor::handle(&handler, invocation)
            .await
            .expect("extension call should succeed");

        let captured_call = captured_call.lock().await.clone().expect("captured call");
        assert!(weak_session.upgrade().is_none());
        assert!(weak_turn.upgrade().is_none());
        assert_eq!(captured_call.turn_id, turn_id);
        assert_eq!(captured_call.call_id, "call-extension");
        assert_eq!(
            captured_call.tool_name,
            codex_tools::ToolName::plain("extension_echo")
        );
        assert_eq!(captured_call.model, expected_model);
        assert_eq!(captured_call.truncation_policy, expected_truncation_policy);
        assert_eq!(
            captured_call.agent_configuration::<Config>(),
            Some(&expected_agent_config)
        );
        assert!(captured_call.turn_activity().is_some());
        assert_eq!(
            captured_call
                .environments
                .iter()
                .map(|environment| environment.file_system_sandbox_context.cwd.clone())
                .collect::<Vec<_>>(),
            expected_sandbox_cwds
        );
        assert_eq!(
            strip_response_item_ids(captured_call.conversation_history.items()),
            vec![expected_history_item]
        );
        match captured_call.payload {
            ToolPayload::Function { arguments } => {
                assert_eq!(arguments, json!({ "message": "hello" }).to_string());
            }
            payload => panic!("expected function payload, got {payload:?}"),
        }

        let started = rx.recv().await.expect("item started event");
        let EventMsg::ItemStarted(started) = started.msg else {
            panic!("expected item started event");
        };
        let TurnItem::Extension(ExtensionItem::WebSearch(started_item)) = started.item else {
            panic!("expected extension web search item");
        };
        assert_eq!(
            started_item,
            WebSearchItem {
                id: "call-extension".to_string(),
                query: String::new(),
                action: None,
                results: None,
            }
        );
    }
}
