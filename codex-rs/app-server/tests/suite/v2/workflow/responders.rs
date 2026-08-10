use super::*;

pub(super) struct WorkflowEvents {
    pub(super) methods: Vec<String>,
    pub(super) started: WorkflowStartedNotification,
    pub(super) progress: Vec<WorkflowProgressNotification>,
    pub(super) completed: WorkflowCompletedNotification,
}

#[derive(Default)]
pub(super) struct WorkflowNotificationPollingResponder {
    pub(super) attempts: AtomicUsize,
}

#[derive(Default)]
pub(super) struct InteractiveWorkflowAgentResponder {
    pub(super) attempts: AtomicUsize,
}

#[derive(Default)]
pub(super) struct RetryControlledWorkflowAgentResponder {
    pub(super) attempts: AtomicUsize,
}

#[derive(Clone, Copy)]
pub(super) struct WorkflowLaunchOutputMatcher;

impl Match for WorkflowLaunchOutputMatcher {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let body = String::from_utf8_lossy(&request.body);
        body.contains(WORKFLOW_CALL_ID) && !body.contains(WAIT_WORKFLOW_CALL_ID)
    }
}

#[derive(Clone, Copy)]
pub(super) struct ToolOutputStageMatcher {
    pub(super) output_call_id: &'static str,
    pub(super) next_call_id: &'static str,
}

impl Match for ToolOutputStageMatcher {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let body = String::from_utf8_lossy(&request.body);
        body.contains(self.output_call_id) && !body.contains(self.next_call_id)
    }
}

#[derive(Clone)]
pub(super) enum MultiWorkflowModelStep {
    LaunchSecond { script: String },
    List,
    WaitAny,
    ReadFirst,
    StopSecond,
    WaitAll,
    WaitFirst,
    WaitFirstAgain,
}

impl Respond for MultiWorkflowModelStep {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().expect("request body should be JSON");
        let first_run_id = || {
            tool_output_from_body(&body, MULTI_WORKFLOW_FIRST_CALL_ID)["runId"]
                .as_str()
                .expect("first Workflow runId")
                .to_string()
        };
        let second_run_id = || {
            tool_output_from_body(&body, MULTI_WORKFLOW_SECOND_CALL_ID)["runId"]
                .as_str()
                .expect("second Workflow runId")
                .to_string()
        };
        let (response_id, call_id, tool_name, arguments) = match self {
            Self::LaunchSecond { script } => (
                "multi-workflow-parent-2",
                MULTI_WORKFLOW_SECOND_CALL_ID,
                "Workflow",
                json!({ "script": script }),
            ),
            Self::List => (
                "multi-workflow-parent-list",
                MULTI_WORKFLOW_LIST_CALL_ID,
                "ListWorkflows",
                json!({ "limit": 10 }),
            ),
            Self::WaitAny => (
                "multi-workflow-parent-3",
                MULTI_WORKFLOW_WAIT_ANY_CALL_ID,
                "WaitWorkflows",
                json!({
                    "runIds": [first_run_id(), second_run_id()],
                    "mode": "any",
                    "timeoutMs": 10_000,
                }),
            ),
            Self::ReadFirst => (
                "multi-workflow-parent-4",
                MULTI_WORKFLOW_READ_FIRST_CALL_ID,
                "ReadWorkflowResult",
                json!({ "runId": first_run_id() }),
            ),
            Self::StopSecond => (
                "multi-workflow-parent-5",
                MULTI_WORKFLOW_STOP_SECOND_CALL_ID,
                "StopWorkflow",
                json!({ "runId": second_run_id() }),
            ),
            Self::WaitAll => (
                "multi-workflow-parent-6",
                MULTI_WORKFLOW_WAIT_ALL_CALL_ID,
                "WaitWorkflows",
                json!({
                    "runIds": [first_run_id(), second_run_id()],
                    "mode": "all",
                    "timeoutMs": 10_000,
                }),
            ),
            Self::WaitFirst => (
                "multi-workflow-parent-7",
                MULTI_WORKFLOW_WAIT_FIRST_CALL_ID,
                "WaitWorkflow",
                json!({ "runId": first_run_id(), "timeoutMs": 10_000 }),
            ),
            Self::WaitFirstAgain => (
                "multi-workflow-parent-8",
                MULTI_WORKFLOW_WAIT_FIRST_AGAIN_CALL_ID,
                "WaitWorkflow",
                json!({ "runId": first_run_id(), "timeoutMs": 10_000 }),
            ),
        };
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created(response_id),
            responses::ev_function_call(call_id, tool_name, &arguments.to_string()),
            responses::ev_completed(response_id),
        ]))
    }
}

pub(super) async fn mount_multi_workflow_step(
    server: &wiremock::MockServer,
    output_call_id: &'static str,
    next_call_id: &'static str,
    step: MultiWorkflowModelStep,
) {
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .and(ToolOutputStageMatcher {
            output_call_id,
            next_call_id,
        })
        .respond_with(step)
        .expect(1)
        .mount(server)
        .await;
}

#[derive(Clone, Copy)]
pub(super) struct WaitWorkflowResponder;

impl Respond for WaitWorkflowResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().expect("request body should be JSON");
        let launch_output = body["input"]
            .as_array()
            .expect("request input array")
            .iter()
            .find(|item| {
                item["type"] == "function_call_output" && item["call_id"] == WORKFLOW_CALL_ID
            })
            .and_then(|item| item["output"].as_str())
            .expect("Workflow function output");
        let launch: serde_json::Value =
            serde_json::from_str(launch_output).expect("Workflow launch JSON");
        let run_id = launch["runId"].as_str().expect("Workflow runId");
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("workflow-wait-parent-2"),
            responses::ev_function_call(
                WAIT_WORKFLOW_CALL_ID,
                "WaitWorkflow",
                &json!({ "runId": run_id, "timeoutMs": 10_000 }).to_string(),
            ),
            responses::ev_completed("workflow-wait-parent-2"),
        ]))
    }
}

#[derive(Clone)]
pub(super) struct LaunchSteerSecondWorkflowResponder {
    pub(super) script: String,
}

impl Respond for LaunchSteerSecondWorkflowResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("workflow-steer-parent-2"),
            responses::ev_function_call(
                STEER_SECOND_WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": &self.script }).to_string(),
            ),
            responses::ev_completed("workflow-steer-parent-2"),
        ]))
    }
}

#[derive(Clone, Copy)]
pub(super) struct LongWaitWorkflowsResponder;

impl Respond for LongWaitWorkflowsResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().expect("request body should be JSON");
        let first_run_id = tool_output_from_body(&body, WORKFLOW_CALL_ID)["runId"]
            .as_str()
            .expect("first Workflow runId")
            .to_string();
        let second_run_id = tool_output_from_body(&body, STEER_SECOND_WORKFLOW_CALL_ID)["runId"]
            .as_str()
            .expect("second Workflow runId")
            .to_string();
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("workflow-steer-parent-3"),
            responses::ev_function_call(
                STEER_WAIT_WORKFLOWS_CALL_ID,
                "WaitWorkflows",
                &json!({
                    "runIds": [first_run_id, second_run_id],
                    "mode": "all",
                    "timeoutMs": 30_000,
                })
                .to_string(),
            ),
            responses::ev_completed("workflow-steer-parent-3"),
        ]))
    }
}

#[derive(Clone, Copy)]
pub(super) struct RepeatLongWaitWorkflowsResponder;

impl Respond for RepeatLongWaitWorkflowsResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().expect("request body should be JSON");
        let first_run_id = tool_output_from_body(&body, WORKFLOW_CALL_ID)["runId"]
            .as_str()
            .expect("first Workflow runId")
            .to_string();
        let second_run_id = tool_output_from_body(&body, STEER_SECOND_WORKFLOW_CALL_ID)["runId"]
            .as_str()
            .expect("second Workflow runId")
            .to_string();
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("workflow-steer-parent-4"),
            responses::ev_function_call(
                STEER_REPEAT_WAIT_WORKFLOWS_CALL_ID,
                "WaitWorkflows",
                &json!({
                    "runIds": [first_run_id, second_run_id],
                    "mode": "all",
                    "timeoutMs": 30_000,
                })
                .to_string(),
            ),
            responses::ev_completed("workflow-steer-parent-4"),
        ]))
    }
}

impl Respond for WorkflowNotificationPollingResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        let response_id = format!("workflow-notify-response-{attempt}");
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created(&response_id),
            responses::ev_assistant_message(
                &format!("workflow-notify-message-{attempt}"),
                "Workflow notification received",
            ),
            responses::ev_completed(&response_id),
        ]))
    }
}

impl Respond for InteractiveWorkflowAgentResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return responses::sse_response(responses::sse(vec![
                responses::ev_response_created("workflow-interactive-child-1"),
                responses::ev_assistant_message(
                    "workflow-interactive-child-message-1",
                    "Initial transcript analysis",
                ),
                responses::ev_completed("workflow-interactive-child-1"),
            ]))
            .set_delay(Duration::from_millis(750));
        }
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("workflow-interactive-child-2"),
            responses::ev_assistant_message(
                "workflow-interactive-child-message-2",
                "Transcript interoperability confirmed",
            ),
            responses::ev_completed_with_tokens("workflow-interactive-child-2", 34),
        ]))
    }
}

impl Respond for RetryControlledWorkflowAgentResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        let response_id = format!("controlled-retry-child-{attempt}");
        let response = responses::sse_response(responses::sse(vec![
            responses::ev_response_created(&response_id),
            responses::ev_assistant_message(&format!("{response_id}-message"), "retry completed"),
            responses::ev_completed_with_tokens(&response_id, 7),
        ]));
        if attempt == 0 {
            response.set_delay(Duration::from_secs(30))
        } else {
            response
        }
    }
}
