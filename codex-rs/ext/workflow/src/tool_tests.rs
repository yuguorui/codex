use super::*;
use codex_config::LoaderOverrides;
use codex_core::ThreadManager;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_extension_api::ConversationHistory;
use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::NoopExtensionEventSink;
use codex_extension_api::ToolApprovalFuture;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolTokenBudget;
use codex_extension_api::TurnItemEmissionFuture;
use codex_extension_api::TurnItemEmitter;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

#[derive(Debug)]
struct ApprovalEmitter {
    decision: ToolApprovalDecision,
    requests: Mutex<Vec<ToolApprovalRequest>>,
    token_budget: Option<Arc<FixedTokenBudget>>,
}

impl ApprovalEmitter {
    fn new(decision: ToolApprovalDecision) -> Self {
        Self {
            decision,
            requests: Mutex::new(Vec::new()),
            token_budget: None,
        }
    }

    fn with_token_budget(decision: ToolApprovalDecision, total: u64, spent: u64) -> Self {
        Self {
            decision,
            requests: Mutex::new(Vec::new()),
            token_budget: Some(Arc::new(FixedTokenBudget { total, spent })),
        }
    }

    fn requests(&self) -> Vec<ToolApprovalRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl TurnItemEmitter for ApprovalEmitter {
    fn emit_started<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn emit_completed<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn request_approval<'a>(&'a self, request: ToolApprovalRequest) -> ToolApprovalFuture<'a> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Box::pin(std::future::ready(self.decision))
    }

    fn token_budget(&self) -> Option<Arc<dyn ToolTokenBudget>> {
        self.token_budget
            .clone()
            .map(|budget| budget as Arc<dyn ToolTokenBudget>)
    }
}

#[derive(Debug)]
struct FixedTokenBudget {
    total: u64,
    spent: u64,
}

impl ToolTokenBudget for FixedTokenBudget {
    fn total(&self) -> u64 {
        self.total
    }

    fn spent(&self) -> u64 {
        self.spent
    }
}

#[tokio::test]
async fn approved_workflow_launches_after_showing_review_details() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Approved));

    let output = fixture
        .executor
        .handle(workflow_call(emitter.clone()))
        .await
        .unwrap();

    let requests = emitter.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header, "Workflow");
    assert!(
        requests[0]
            .question
            .contains("Review dynamic workflow before running")
    );
    assert!(requests[0].question.contains("Name: approval-test"));
    assert!(requests[0].question.contains("Phases: Inspect, Verify"));
    assert!(
        requests[0]
            .question
            .contains("Source: inline script supplied by the model")
    );
    assert!(requests[0].question.contains("SHA-256:"));
    assert!(requests[0].question.contains("Script (complete):"));
    let result = output.code_mode_result(&workflow_payload());
    assert_eq!(result["status"], "async_launched");
    assert_eq!(fixture.service.list(fixture.thread_id).len(), 1);
    fixture.wait_for_terminal().await;
}

#[tokio::test]
async fn denied_workflow_does_not_create_a_task() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Denied));

    let result = fixture
        .executor
        .handle(workflow_call(emitter.clone()))
        .await;

    let Err(error) = result else {
        panic!("denied workflow should fail before launch");
    };
    assert_eq!(
        error,
        FunctionCallError::RespondToModel("dynamic workflow was not approved".to_string())
    );
    assert_eq!(emitter.requests().len(), 1);
    assert!(fixture.service.list(fixture.thread_id).is_empty());
}

#[tokio::test]
async fn invalid_agent_prompt_is_rejected_before_approval_or_launch() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Approved));
    let source = "export const meta = { name: 'invalid-prompt', description: 'invalid prompt' };\nreturn agent(['review this', 'carefully']);";

    let result = fixture
        .executor
        .handle(workflow_call_with_payload(
            emitter.clone(),
            workflow_payload_with_source(source),
        ))
        .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("invalid agent prompt should fail before launch");
    };
    assert_eq!(
        message,
        "workflow script has an invalid `agent()` prompt at line 2, column 14: the prompt is statically not a string"
    );
    assert!(emitter.requests().is_empty());
    assert!(fixture.service.list(fixture.thread_id).is_empty());
}

#[tokio::test]
async fn never_approval_policy_launches_without_prompting() {
    let fixture = ToolFixture::new(AskForApproval::Never).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Denied));

    let output = fixture
        .executor
        .handle(workflow_call(emitter.clone()))
        .await
        .unwrap();

    assert!(emitter.requests().is_empty());
    assert_eq!(
        output.code_mode_result(&workflow_payload())["status"],
        "async_launched"
    );
    assert_eq!(fixture.service.list(fixture.thread_id).len(), 1);
    fixture.wait_for_terminal().await;
}

#[tokio::test]
async fn tool_shared_budget_reaches_runtime_and_blocks_agents_at_the_ceiling() {
    let fixture = ToolFixture::new(AskForApproval::Never).await;
    let emitter = Arc::new(ApprovalEmitter::with_token_budget(
        ToolApprovalDecision::Denied,
        40,
        40,
    ));
    let source = r#"export const meta = { name: 'budget-test', description: 'exercise the host budget' };
const results = await parallel([() => agent('must not reach the agent runner')]);
return [results[0], budget.total, budget.spent(), budget.remaining()];"#;

    let output = fixture
        .executor
        .handle(workflow_call_with_payload(
            emitter,
            workflow_payload_with_source(source),
        ))
        .await
        .unwrap();

    assert_eq!(
        output.code_mode_result(&workflow_payload_with_source(source))["status"],
        "async_launched"
    );
    fixture.wait_for_terminal().await;
    let snapshots = fixture.service.list(fixture.thread_id);
    let [snapshot] = snapshots.as_slice() else {
        panic!("workflow should remain in service history");
    };
    assert_eq!(snapshot.status, WorkflowTaskStatus::Completed);
    assert_eq!(snapshot.result, json!([null, 40, 40, 0]));
    assert_eq!(snapshot.usage.agent_count, 0);
}

#[tokio::test]
async fn large_approval_preview_is_segmented_hashed_and_marked_incomplete() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Denied));
    let source = format!(
        "export const meta = {{ name: 'large-preview', description: 'large' }};\n/*{}MIDDLE-MARKER{}*/\nreturn 'TAIL-MARKER';",
        "界".repeat(1_000),
        "x".repeat(3_000),
    );
    let expected_hash = format!("{:x}", Sha256::digest(source.as_bytes()));

    let _ = fixture
        .executor
        .handle(workflow_call_with_payload(
            emitter.clone(),
            workflow_payload_with_source(&source),
        ))
        .await;

    let request = emitter.requests().pop().unwrap();
    assert!(request.question.contains("Script preview (INCOMPLETE:"));
    assert!(request.question.contains("bytes omitted"));
    assert!(
        request
            .question
            .contains(&format!("SHA-256: {expected_hash}"))
    );
    assert!(request.question.contains("TAIL-MARKER"));
    assert!(!request.question.contains("MIDDLE-MARKER"));
}

#[tokio::test]
async fn approval_discloses_project_shadowing_and_resolved_path() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Denied));
    let path = fixture
        ._codex_home
        .path()
        .join(".codex/workflows/deep-research.js");
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(
        &path,
        "export const meta = { name: 'shadow', description: 'shadow test' }; return null",
    )
    .await
    .unwrap();

    let _ = fixture
        .executor
        .handle(workflow_call_with_payload(
            emitter.clone(),
            ToolPayload::Function {
                arguments: json!({ "name": "deep-research" }).to_string(),
            },
        ))
        .await;

    let request = emitter.requests().pop().unwrap();
    assert!(request.question.contains(&path.display().to_string()));
    assert!(
        request
            .question
            .contains("Warning: this file shadows a lower-priority workflow with the same name.")
    );
}

struct ToolFixture {
    _codex_home: tempfile::TempDir,
    thread_id: ThreadId,
    service: WorkflowService,
    executor: WorkflowToolExecutor,
}

impl ToolFixture {
    async fn new(approval_policy: AskForApproval) -> Self {
        let codex_home = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .harness_overrides(ConfigOverrides {
                approval_policy: Some(approval_policy),
                ..ConfigOverrides::default()
            })
            .build()
            .await
            .unwrap();
        let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
        let service = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
        let executor = WorkflowToolExecutor::new(
            thread_id,
            config,
            service.clone(),
            AgentRunner::new(Weak::<ThreadManager>::new()),
            Weak::<ThreadManager>::new(),
        );
        Self {
            _codex_home: codex_home,
            thread_id,
            service,
            executor,
        }
    }

    async fn wait_for_terminal(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let terminal = self
                    .service
                    .list(self.thread_id)
                    .first()
                    .is_some_and(|snapshot| {
                        matches!(
                            snapshot.status,
                            WorkflowTaskStatus::Completed
                                | WorkflowTaskStatus::Failed
                                | WorkflowTaskStatus::Paused
                                | WorkflowTaskStatus::Killed
                        )
                    });
                if terminal {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background workflow should settle");
    }
}

fn workflow_call(emitter: Arc<dyn TurnItemEmitter>) -> ToolCall {
    workflow_call_with_payload(emitter, workflow_payload())
}

fn workflow_call_with_payload(emitter: Arc<dyn TurnItemEmitter>, payload: ToolPayload) -> ToolCall {
    ToolCall {
        turn_id: "turn-1".to_string(),
        call_id: "call-workflow".to_string(),
        tool_name: ToolName::plain(WORKFLOW_TOOL_NAME),
        model: "gpt-test".to_string(),
        codex_turn_metadata: None,
        truncation_policy: TruncationPolicy::Bytes(1024),
        conversation_history: ConversationHistory::default(),
        turn_item_emitter: emitter,
        environments: Vec::new(),
        payload,
    }
}

fn workflow_payload() -> ToolPayload {
    workflow_payload_with_source(
        "export const meta = { name: 'approval-test', description: 'Review this script before launch', phases: [{ title: 'Inspect' }, { title: 'Verify' }] }; return 'ok'",
    )
}

fn workflow_payload_with_source(source: &str) -> ToolPayload {
    ToolPayload::Function {
        arguments: json!({
            "script": source,
            "args": { "target": "src/lib.rs" }
        })
        .to_string(),
    }
}
