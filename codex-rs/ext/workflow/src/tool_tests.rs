use super::*;
use crate::service::WorkflowTaskSnapshot;
use codex_config::LoaderOverrides;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerEnvVar;
use codex_config::types::McpServerOAuthConfig;
use codex_config::types::McpServerTransportConfig;
use codex_core::ThreadManager;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_extension_api::ConversationHistory;
use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::NoopExtensionEventSink;
use codex_extension_api::ToolApprovalDecision;
use codex_extension_api::ToolApprovalDenialSource;
use codex_extension_api::ToolApprovalFuture;
use codex_extension_api::ToolApprovalOutcome;
use codex_extension_api::ToolApprovalOutcomeFuture;
use codex_extension_api::ToolApprovalReviewRequest;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolPayload;
use codex_extension_api::TurnItemEmissionFuture;
use codex_extension_api::TurnItemEmitter;
use codex_file_system::FileSystemSandboxContext;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

async fn read_snapshot_result(snapshot: &WorkflowTaskSnapshot) -> serde_json::Value {
    let artifact = snapshot
        .result_artifact
        .as_ref()
        .expect("terminal result artifact");
    crate::result_artifact::validate_result_artifact(&snapshot.output_file, artifact)
        .await
        .unwrap();
    let mut offset = 0;
    let mut serialized = String::new();
    while offset < artifact.bytes {
        let chunk = crate::result_artifact::read_result_artifact_chunk(
            &snapshot.output_file,
            artifact,
            offset,
            4 * 1024,
        )
        .await
        .unwrap();
        serialized.push_str(&chunk.text);
        offset = chunk.next_offset;
    }
    serde_json::from_str(&serialized).unwrap()
}

#[derive(Debug)]
struct ApprovalEmitter {
    decision: ToolApprovalDecision,
    requests: Mutex<Vec<ToolApprovalRequest>>,
}

impl ApprovalEmitter {
    fn new(decision: ToolApprovalDecision) -> Self {
        Self {
            decision,
            requests: Mutex::new(Vec::new()),
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
}

#[derive(Debug)]
struct DetailedApprovalEmitter {
    outcome: ToolApprovalOutcome,
    requests: Mutex<Vec<ToolApprovalReviewRequest>>,
    user_outcome: ToolApprovalOutcome,
    user_requests: Mutex<Vec<ToolApprovalRequest>>,
    review_mode: ToolApprovalReviewMode,
}

impl DetailedApprovalEmitter {
    fn new(outcome: ToolApprovalOutcome) -> Self {
        Self {
            outcome,
            requests: Mutex::new(Vec::new()),
            user_outcome: ToolApprovalOutcome::Unavailable,
            user_requests: Mutex::new(Vec::new()),
            review_mode: ToolApprovalReviewMode::User,
        }
    }

    fn automatic(
        review_mode: ToolApprovalReviewMode,
        outcome: ToolApprovalOutcome,
        user_outcome: ToolApprovalOutcome,
    ) -> Self {
        Self {
            outcome,
            requests: Mutex::new(Vec::new()),
            user_outcome,
            user_requests: Mutex::new(Vec::new()),
            review_mode,
        }
    }

    fn requests(&self) -> Vec<ToolApprovalReviewRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn user_requests(&self) -> Vec<ToolApprovalRequest> {
        self.user_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl TurnItemEmitter for DetailedApprovalEmitter {
    fn emit_started<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn emit_completed<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn request_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalReviewRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Box::pin(std::future::ready(self.outcome.clone()))
    }

    fn request_user_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        self.user_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Box::pin(std::future::ready(self.user_outcome.clone()))
    }

    fn approval_review_mode(&self) -> ToolApprovalReviewMode {
        self.review_mode
    }
}

#[derive(Debug)]
struct ArtifactReviewingApprovalEmitter {
    codex_home: codex_utils_absolute_path::AbsolutePathBuf,
    thread_id: ThreadId,
    expected_source: String,
    reviewed: Mutex<bool>,
    requests: Mutex<Vec<ToolApprovalReviewRequest>>,
    user_requests: Mutex<Vec<ToolApprovalRequest>>,
}

impl ArtifactReviewingApprovalEmitter {
    fn new(
        codex_home: codex_utils_absolute_path::AbsolutePathBuf,
        thread_id: ThreadId,
        expected_source: String,
    ) -> Self {
        Self {
            codex_home,
            thread_id,
            expected_source,
            reviewed: Mutex::new(false),
            requests: Mutex::new(Vec::new()),
            user_requests: Mutex::new(Vec::new()),
        }
    }

    fn reviewed(&self) -> bool {
        *self
            .reviewed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn requests(&self) -> Vec<ToolApprovalReviewRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn user_requests(&self) -> Vec<ToolApprovalRequest> {
        self.user_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn review_artifact(&self, request: &ToolApprovalReviewRequest) -> Result<(), String> {
        let artifact = request
            .artifact
            .as_ref()
            .ok_or_else(|| "approval request did not include an artifact".to_string())?;
        if !artifact.has_valid_sha256() {
            return Err("approval artifact hash is invalid".to_string());
        }

        let mut reviewed_contents = String::new();
        let mut offset = 0;
        let mut page_count = 0;
        loop {
            let page = read_workflow_approval_artifact(
                &self.codex_home,
                self.thread_id,
                artifact.sha256(),
                offset,
            )
            .await?;
            if page.sha256 != artifact.sha256() || page.offset != offset {
                return Err("approval artifact page identity is inconsistent".to_string());
            }
            reviewed_contents.push_str(&page.contents);
            page_count += 1;
            let Some(next_offset) = page.next_offset else {
                break;
            };
            offset = next_offset;
        }
        if page_count < 2 {
            return Err("approval artifact was not reviewed through pagination".to_string());
        }
        if reviewed_contents != artifact.contents() {
            return Err("approval artifact pages did not reconstruct the bound action".to_string());
        }

        let reviewed_action: serde_json::Value =
            serde_json::from_str(&reviewed_contents).map_err(|error| error.to_string())?;
        if reviewed_action != request.action {
            return Err("approval artifact does not match the requested action".to_string());
        }
        if reviewed_action["reviewedScript"]["source"].as_str()
            != Some(self.expected_source.as_str())
        {
            return Err("approval artifact does not contain the complete source".to_string());
        }
        let source_sha256 = format!("{:x}", Sha256::digest(self.expected_source.as_bytes()));
        if reviewed_action["reviewedScript"]["sha256"].as_str() != Some(source_sha256.as_str()) {
            return Err("approval artifact source hash is invalid".to_string());
        }
        Ok(())
    }
}

impl TurnItemEmitter for ArtifactReviewingApprovalEmitter {
    fn emit_started<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn emit_completed<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn request_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalReviewRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            match self.review_artifact(&request).await {
                Ok(()) => {
                    *self
                        .reviewed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                    ToolApprovalOutcome::Approved
                }
                Err(rejection) => ToolApprovalOutcome::Denied {
                    rejection,
                    source: ToolApprovalDenialSource::AutomaticReviewer,
                },
            }
        })
    }

    fn request_user_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        self.user_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Box::pin(std::future::ready(ToolApprovalOutcome::Unavailable))
    }

    fn approval_review_mode(&self) -> ToolApprovalReviewMode {
        ToolApprovalReviewMode::StrictAutomatic
    }
}

#[derive(Debug)]
struct ReplacingArtifactApprovalEmitter {
    approval_dir: PathBuf,
}

impl TurnItemEmitter for ReplacingArtifactApprovalEmitter {
    fn emit_started<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn emit_completed<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn request_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalReviewRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        let action = canonical_json_value(request.action);
        let contents = serde_json::to_string_pretty(&action).unwrap();
        let sha256 = format!("{:x}", Sha256::digest(contents.as_bytes()));
        std::fs::write(self.approval_dir.join(format!("{sha256}.json")), b"{}").unwrap();
        Box::pin(std::future::ready(ToolApprovalOutcome::Approved))
    }
}

#[derive(Debug)]
struct MutatingApprovalEmitter {
    child_path: PathBuf,
    replacement_source: String,
    requests: Mutex<Vec<ToolApprovalReviewRequest>>,
}

impl MutatingApprovalEmitter {
    fn requests(&self) -> Vec<ToolApprovalReviewRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl TurnItemEmitter for MutatingApprovalEmitter {
    fn emit_started<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn emit_completed<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn request_approval_detailed<'a>(
        &'a self,
        request: ToolApprovalReviewRequest,
    ) -> ToolApprovalOutcomeFuture<'a> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        std::fs::write(&self.child_path, &self.replacement_source).unwrap();
        Box::pin(std::future::ready(ToolApprovalOutcome::Approved))
    }
}

#[tokio::test]
async fn approved_workflow_launches_after_showing_review_details() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Approved));

    let output = fixture
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
    assert_eq!(
        fixture.service.list(fixture.thread_id).await.unwrap().len(),
        1
    );
    fixture.wait_for_terminal().await;
}

#[tokio::test]
async fn top_level_script_path_launches_file_workflow() {
    let fixture = ToolFixture::new(AskForApproval::Never).await;
    let script_path = fixture.config.cwd.join("file-workflow.js");
    let inputs_dir = fixture.config.cwd.join("inputs");
    tokio::fs::create_dir_all(&inputs_dir).await.unwrap();
    tokio::fs::write(inputs_dir.join("input.txt"), "frozen input")
        .await
        .unwrap();
    tokio::fs::write(
        &script_path,
        "export const meta = { name: 'file-workflow', description: 'file workflow', inputs: ['inputs/*.txt'] }; return { files: await listInputs(), content: await readInput('inputs/input.txt') }",
    )
    .await
    .unwrap();
    let payload = ToolPayload::Function {
        arguments: json!({ "scriptPath": "file-workflow.js" }).to_string(),
    };
    let invocation = workflow_call_with_payload(
        Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Approved)),
        payload.clone(),
    );
    let input = serde_json::from_str(invocation.function_arguments().unwrap()).unwrap();

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.executor.handle_with_context(
            invocation,
            input,
            local_workflow_execution_context(&fixture.config),
        ),
    )
    .await
    .expect("Workflow tool should return after launching a file-backed workflow")
    .unwrap();
    assert_eq!(
        output.code_mode_result(&payload)["status"],
        "async_launched"
    );
    fixture.wait_for_terminal().await;

    let snapshots = fixture.service.list(fixture.thread_id).await.unwrap();
    let [snapshot] = snapshots.as_slice() else {
        panic!("expected one workflow task");
    };
    assert_eq!(snapshot.status, WorkflowTaskStatus::Completed);
    assert_eq!(
        read_snapshot_result(snapshot).await,
        json!({
            "files": [{
                "path": "inputs/input.txt",
                "bytes": 12,
                "sha256": format!("{:x}", Sha256::digest(b"frozen input")),
            }],
            "content": "frozen input",
        })
    );
}

#[tokio::test]
async fn top_level_inline_workflow_reads_frozen_declared_input() {
    let fixture = ToolFixture::new(AskForApproval::Never).await;
    let input_path = fixture.config.cwd.join("inline-input.txt");
    tokio::fs::write(&input_path, "revision one").await.unwrap();
    let source = r#"export const meta = { name: 'inline-input', description: 'inline declared input', inputs: ['inline-input.txt'] };
await new Promise((resolve) => setTimeout(resolve, 100));
return { files: await listInputs(), content: await readInput('inline-input.txt') };"#;
    let payload = workflow_payload_with_source(source);
    let invocation = workflow_call_with_payload(
        Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Approved)),
        payload.clone(),
    );
    let input = serde_json::from_str(invocation.function_arguments().unwrap()).unwrap();

    let output = fixture
        .executor
        .handle_with_context(
            invocation,
            input,
            local_workflow_execution_context(&fixture.config),
        )
        .await
        .unwrap();
    assert_eq!(
        output.code_mode_result(&payload)["status"],
        "async_launched"
    );
    tokio::fs::write(&input_path, "revision two").await.unwrap();
    fixture.wait_for_terminal().await;

    let snapshots = fixture.service.list(fixture.thread_id).await.unwrap();
    let [snapshot] = snapshots.as_slice() else {
        panic!("expected one workflow task");
    };
    assert_eq!(snapshot.status, WorkflowTaskStatus::Completed);
    assert_eq!(
        read_snapshot_result(snapshot).await,
        json!({
            "files": [{
                "path": "inline-input.txt",
                "bytes": 12,
                "sha256": format!("{:x}", Sha256::digest(b"revision one")),
            }],
            "content": "revision one",
        })
    );
}

#[tokio::test]
async fn approval_action_exposes_declared_input_manifest_without_content() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let secret = "DECLARED_INPUT_CONTENT_MUST_STAY_FROZEN";
    tokio::fs::write(fixture.config.cwd.join("approval-input.txt"), secret)
        .await
        .unwrap();
    let source = "export const meta = { name: 'approval-input', description: 'approval manifest', inputs: ['approval-input.txt'] }; return readInput('approval-input.txt')";
    let payload = workflow_payload_with_source(source);
    let emitter = Arc::new(DetailedApprovalEmitter::new(ToolApprovalOutcome::Approved));
    let invocation = workflow_call_with_payload(emitter.clone(), payload);
    let input = serde_json::from_str(invocation.function_arguments().unwrap()).unwrap();

    fixture
        .executor
        .handle_with_context(
            invocation,
            input,
            local_workflow_execution_context(&fixture.config),
        )
        .await
        .unwrap();

    let requests = emitter.requests();
    let [request] = requests.as_slice() else {
        panic!("expected one approval request");
    };
    assert_eq!(
        request.action["declaredInputs"],
        json!([{
            "path": "approval-input.txt",
            "bytes": secret.len(),
            "sha256": format!("{:x}", Sha256::digest(secret.as_bytes())),
        }])
    );
    assert!(!request.action.to_string().contains(secret));
    fixture.wait_for_terminal().await;
}

#[tokio::test]
async fn child_workflow_reads_its_declared_input_from_the_frozen_composition() {
    let fixture = ToolFixture::new(AskForApproval::Never).await;
    tokio::fs::write(
        fixture.config.cwd.join("child.js"),
        "export const meta = { name: 'input-child', description: 'child input', inputs: ['child-input.txt'] }; return readInput('child-input.txt')",
    )
    .await
    .unwrap();
    tokio::fs::write(
        fixture.config.cwd.join("child-input.txt"),
        "child frozen input",
    )
    .await
    .unwrap();
    let source = "export const meta = { name: 'input-parent', description: 'parent input' }; return workflow({ scriptPath: 'child.js' })";
    let payload = workflow_payload_with_source(source);
    let invocation = workflow_call_with_payload(
        Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Approved)),
        payload,
    );
    let input = serde_json::from_str(invocation.function_arguments().unwrap()).unwrap();

    fixture
        .executor
        .handle_with_context(
            invocation,
            input,
            local_workflow_execution_context(&fixture.config),
        )
        .await
        .unwrap();
    fixture.wait_for_terminal().await;

    let snapshots = fixture.service.list(fixture.thread_id).await.unwrap();
    let [snapshot] = snapshots.as_slice() else {
        panic!("expected one workflow task");
    };
    assert_eq!(snapshot.status, WorkflowTaskStatus::Completed);
    assert_eq!(
        read_snapshot_result(snapshot).await,
        json!("child frozen input")
    );
}

#[tokio::test]
async fn detailed_approval_reviews_complete_script_and_canonical_arguments() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(DetailedApprovalEmitter::new(ToolApprovalOutcome::Approved));

    let output = fixture
        .handle(workflow_call(emitter.clone()))
        .await
        .unwrap();

    let requests = emitter.requests();
    let [request] = requests.as_slice() else {
        panic!("expected one detailed approval request");
    };
    assert_eq!(
        request.action["reviewedScript"]["source"],
        "export const meta = { name: 'approval-test', description: 'Review this script before launch', phases: [{ title: 'Inspect' }, { title: 'Verify' }] }; return 'ok'"
    );
    assert_eq!(request.action["reviewedScript"]["kind"], "completeSource");
    assert!(request.action["reviewedScript"]["sha256"].is_string());
    assert_eq!(
        request.action["arguments"],
        json!({ "target": "src/lib.rs" })
    );
    assert_eq!(
        serde_json::to_string(&request.action["arguments"]).unwrap(),
        r#"{"target":"src/lib.rs"}"#
    );
    assert!(
        request
            .prompt
            .question
            .contains("Arguments (canonical, complete):")
    );
    assert_eq!(
        output.code_mode_result(&workflow_payload())["status"],
        "async_launched"
    );
    fixture.wait_for_terminal().await;
}

#[tokio::test]
async fn approval_artifact_contains_only_redacted_child_capabilities() {
    let mut fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let role_secret = "WORKFLOW_ROLE_LAYER_SECRET";
    let role_path = fixture.config.cwd.join("secret-role.toml");
    tokio::fs::write(
        &role_path,
        format!("developer_instructions = {role_secret:?}\n"),
    )
    .await
    .unwrap();
    fixture.config.agent_roles.insert(
        "secret-role".to_string(),
        codex_core::config::AgentRoleConfig {
            description: Some("Secret role".to_string()),
            config_file: Some(role_path.to_path_buf()),
            nickname_candidates: None,
        },
    );
    fixture.config.base_instructions = Some("WORKFLOW_BASE_INSTRUCTIONS_SECRET".to_string());
    let mut servers = fixture.config.mcp_servers.get().clone();
    servers.insert(
        "redacted-server".to_string(),
        McpServerConfig {
            transport: McpServerTransportConfig::StreamableHttp {
                url: "https://WORKFLOW_MCP_URL_SECRET.invalid".to_string(),
                bearer_token_env_var: Some("WORKFLOW_MCP_BEARER_ENV_SECRET".to_string()),
                http_headers: Some(HashMap::from([(
                    "Authorization".to_string(),
                    "WORKFLOW_MCP_HEADER_SECRET".to_string(),
                )])),
                env_http_headers: Some(HashMap::from([(
                    "X-Env".to_string(),
                    "WORKFLOW_MCP_HEADER_ENV_SECRET".to_string(),
                )])),
                http_headers_helper: Some("WORKFLOW_MCP_HELPER_SECRET".to_string()),
            },
            auth: Default::default(),
            environment_id: "local".to_string(),
            enabled: false,
            required: false,
            supports_parallel_tool_calls: false,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: Some(vec!["WORKFLOW_ALLOWED_TOOL_SECRET".to_string()]),
            disabled_tools: Some(vec!["WORKFLOW_DENIED_TOOL_SECRET".to_string()]),
            scopes: Some(vec!["WORKFLOW_MCP_SCOPE_SECRET".to_string()]),
            oauth: Some(McpServerOAuthConfig {
                client_id: Some("WORKFLOW_MCP_OAUTH_SECRET".to_string()),
                callback_url: None,
                callback_port: Some(32123),
            }),
            oauth_resource: Some("WORKFLOW_MCP_RESOURCE_SECRET".to_string()),
            tools: HashMap::new(),
        },
    );
    servers.insert(
        "redacted-stdio-server".to_string(),
        McpServerConfig {
            transport: McpServerTransportConfig::Stdio {
                command: "WORKFLOW_MCP_COMMAND_SECRET".to_string(),
                args: vec!["WORKFLOW_MCP_ARGUMENT_SECRET".to_string()],
                env: Some(HashMap::from([(
                    "WORKFLOW_MCP_ENV_NAME_SECRET".to_string(),
                    "WORKFLOW_MCP_ENV_VALUE_SECRET".to_string(),
                )])),
                env_vars: vec![McpServerEnvVar::from("WORKFLOW_MCP_ENV_VAR_SECRET")],
                cwd: None,
            },
            auth: Default::default(),
            environment_id: "local".to_string(),
            enabled: false,
            required: false,
            supports_parallel_tool_calls: false,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );
    fixture.config.mcp_servers.set(servers).unwrap();
    let emitter = Arc::new(DetailedApprovalEmitter::new(ToolApprovalOutcome::Denied {
        rejection: "stop after inspection".to_string(),
        source: ToolApprovalDenialSource::AutomaticReviewer,
    }));

    let _ = fixture.handle(workflow_call(emitter.clone())).await;

    let request = emitter.requests().pop().expect("approval request");
    let artifact = request.artifact.expect("hash-bound approval artifact");
    let serialized_action = request.action.to_string();
    assert_eq!(
        artifact.contents(),
        serde_json::to_string_pretty(&canonical_json_value(request.action)).unwrap()
    );
    for secret in [
        role_secret,
        "WORKFLOW_BASE_INSTRUCTIONS_SECRET",
        "WORKFLOW_MCP_URL_SECRET",
        "WORKFLOW_MCP_BEARER_ENV_SECRET",
        "WORKFLOW_MCP_HEADER_SECRET",
        "WORKFLOW_MCP_HEADER_ENV_SECRET",
        "WORKFLOW_MCP_HELPER_SECRET",
        "WORKFLOW_ALLOWED_TOOL_SECRET",
        "WORKFLOW_DENIED_TOOL_SECRET",
        "WORKFLOW_MCP_SCOPE_SECRET",
        "WORKFLOW_MCP_OAUTH_SECRET",
        "WORKFLOW_MCP_RESOURCE_SECRET",
        "WORKFLOW_MCP_COMMAND_SECRET",
        "WORKFLOW_MCP_ARGUMENT_SECRET",
        "WORKFLOW_MCP_ENV_NAME_SECRET",
        "WORKFLOW_MCP_ENV_VALUE_SECRET",
        "WORKFLOW_MCP_ENV_VAR_SECRET",
    ] {
        assert!(
            !serialized_action.contains(secret),
            "action leaked {secret}"
        );
        assert!(
            !artifact.contents().contains(secret),
            "artifact leaked {secret}"
        );
    }
    assert!(serialized_action.contains("redacted-server"));
    assert!(serialized_action.contains("redacted-stdio-server"));
    assert!(serialized_action.contains("configLayerSha256"));
}

#[tokio::test]
async fn replaced_approval_artifact_fails_before_launch() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let approval_dir = workflow_session_dir(&fixture.config.codex_home, fixture.thread_id)
        .join("workflows/approvals")
        .to_path_buf();
    let emitter = Arc::new(ReplacingArtifactApprovalEmitter { approval_dir });

    let result = fixture.handle(workflow_call(emitter)).await;

    assert_eq!(
        result.err(),
        Some(FunctionCallError::RespondToModel(
            "the persisted Workflow approval action changed before execution".to_string()
        ))
    );
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn approval_and_runtime_use_the_same_frozen_named_child_after_toctou_change() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let child_path = fixture.config.cwd.join(".codex/workflows/child.js");
    tokio::fs::create_dir_all(child_path.parent().unwrap())
        .await
        .unwrap();
    let approved_child =
        "export const meta = { name: 'child', description: 'approved child' }; return 'approved'";
    let replacement_child =
        "export const meta = { name: 'child', description: 'changed child' }; return 'changed'";
    tokio::fs::write(&child_path, approved_child).await.unwrap();
    let emitter = Arc::new(MutatingApprovalEmitter {
        child_path: child_path.to_path_buf(),
        replacement_source: replacement_child.to_string(),
        requests: Mutex::new(Vec::new()),
    });
    let parent = "export const meta = { name: 'parent', description: 'frozen parent' }; return workflow('child')";

    fixture
        .handle(workflow_call_with_payload(
            emitter.clone(),
            workflow_payload_with_source(parent),
        ))
        .await
        .unwrap();
    fixture.wait_for_terminal().await;

    let requests = emitter.requests();
    let [request] = requests.as_slice() else {
        panic!("expected one approval request");
    };
    assert!(request.action["definitionSha256"].is_string());
    assert_eq!(
        request.action["reviewedChildren"][0]["binding"],
        json!({ "kind": "name", "name": "child" })
    );
    assert_eq!(
        request.action["reviewedChildren"][0]["reviewedScript"]["source"],
        approved_child
    );
    assert!(
        request.action["reviewedChildren"][0]["scriptSha256"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
    );
    let snapshots = fixture.service.list(fixture.thread_id).await.unwrap();
    let [snapshot] = snapshots.as_slice() else {
        panic!("expected one workflow task");
    };
    assert_eq!(snapshot.status, WorkflowTaskStatus::Completed);
    assert_eq!(read_snapshot_result(snapshot).await, json!("approved"));
}

#[tokio::test]
async fn static_script_path_child_executes_from_the_frozen_composition() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let child_path = fixture.config.cwd.join("child.js");
    tokio::fs::write(
        &child_path,
        "export const meta = { name: 'path-child', description: 'path child' }; return 'path-result'",
    )
    .await
    .unwrap();
    let emitter = Arc::new(DetailedApprovalEmitter::new(ToolApprovalOutcome::Approved));
    let parent = "export const meta = { name: 'parent', description: 'path parent' }; return workflow({ scriptPath: 'child.js' })";

    fixture
        .handle(workflow_call_with_payload(
            emitter,
            workflow_payload_with_source(parent),
        ))
        .await
        .unwrap();
    fixture.wait_for_terminal().await;

    let snapshots = fixture.service.list(fixture.thread_id).await.unwrap();
    let [snapshot] = snapshots.as_slice() else {
        panic!("expected one workflow task");
    };
    assert_eq!(snapshot.status, WorkflowTaskStatus::Completed);
    assert_eq!(read_snapshot_result(snapshot).await, json!("path-result"));
}

#[tokio::test]
async fn remote_child_composition_is_rejected_before_approval() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(DetailedApprovalEmitter::new(ToolApprovalOutcome::Approved));
    let parent = "export const meta = { name: 'remote-parent', description: 'remote child' }; return workflow('child')";
    let invocation =
        workflow_call_with_payload(emitter.clone(), workflow_payload_with_source(parent));
    let input = serde_json::from_str(invocation.function_arguments().unwrap()).unwrap();

    let result = fixture
        .executor
        .handle_with_context(
            invocation,
            input,
            WorkflowExecutionContext {
                config: fixture.config.clone(),
                environments: Vec::new(),
                tool_environments: Vec::new(),
                captured_environments: None,
                execution_environment_action: json!({}),
                location: WorkflowEnvironmentLocation::Remote,
                script_access: WorkflowScriptAccess::InlineOnly,
            },
        )
        .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("remote child composition must fail closed");
    };
    assert!(
        message.contains(
            "run child workflow composition with a local execution environment filesystem"
        )
    );
    assert!(emitter.requests().is_empty());
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn nested_child_composition_is_rejected_before_approval() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let child_path = fixture.config.cwd.join(".codex/workflows/child.js");
    tokio::fs::create_dir_all(child_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(
        child_path,
        "export const meta = { name: 'child', description: 'nested child' }; return workflow('grandchild')",
    )
    .await
    .unwrap();
    let emitter = Arc::new(DetailedApprovalEmitter::new(ToolApprovalOutcome::Approved));
    let parent = "export const meta = { name: 'parent', description: 'nested composition' }; return workflow('child')";

    let result = fixture
        .handle(workflow_call_with_payload(
            emitter.clone(),
            workflow_payload_with_source(parent),
        ))
        .await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("nested child composition must fail closed");
    };
    assert!(message.starts_with("invalid child workflow composition:"));
    assert!(message.contains("call child workflow `child` directly from the root workflow"));
    assert!(emitter.requests().is_empty());
    assert!(emitter.user_requests().is_empty());
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn bundled_workflow_approval_uses_trusted_hash_instead_of_complete_source() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(DetailedApprovalEmitter::new(ToolApprovalOutcome::Denied {
        rejection: "review fixture".to_string(),
        source: ToolApprovalDenialSource::AutomaticReviewer,
    }));

    let result = fixture
        .handle(workflow_call_with_payload(
            emitter.clone(),
            ToolPayload::Function {
                arguments: json!({ "name": "deep-research", "args": "test" }).to_string(),
            },
        ))
        .await;

    assert!(result.is_err());
    let requests = emitter.requests();
    let [request] = requests.as_slice() else {
        panic!("expected one detailed approval request");
    };
    assert_eq!(request.action["origin"], "bundled workflow");
    assert_eq!(request.action["reviewedScript"]["kind"], "trustedBundled");
    assert!(request.action["reviewedScript"]["sha256"].is_string());
    assert!(request.action["reviewedScript"].get("source").is_none());
    assert!(serde_json::to_vec(&request.action).unwrap().len() < 8_000);
}

#[tokio::test]
async fn denied_workflow_does_not_create_a_task() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Denied));

    let result = fixture.handle(workflow_call(emitter.clone())).await;

    let Err(error) = result else {
        panic!("denied workflow should fail before launch");
    };
    assert_eq!(
        error,
        FunctionCallError::RespondToModel("dynamic workflow was not approved".to_string())
    );
    assert_eq!(emitter.requests().len(), 1);
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn detailed_denials_preserve_source_and_bound_the_owning_model_message() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(DetailedApprovalEmitter::new(ToolApprovalOutcome::Denied {
        rejection: "unsafe action ".repeat(10_000),
        source: ToolApprovalDenialSource::AutomaticReviewer,
    }));

    let result = fixture.handle(workflow_call(emitter)).await;

    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("automatic denial should prevent launch");
    };
    assert!(message.starts_with("automatic approval review denied the dynamic workflow:"));
    assert!(codex_utils_output_truncation::approx_token_count(&message) <= 900);
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn detailed_user_denial_carries_the_rejection_reason() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(DetailedApprovalEmitter::new(ToolApprovalOutcome::Denied {
        rejection: "the requested destination is outside the approved scope".to_string(),
        source: ToolApprovalDenialSource::User,
    }));

    let result = fixture.handle(workflow_call(emitter)).await;

    let Err(error) = result else {
        panic!("user denial should prevent launch");
    };
    assert_eq!(
        error,
        FunctionCallError::RespondToModel(
            "the user denied the dynamic workflow: the requested destination is outside the approved scope"
                .to_string()
        )
    );
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn detailed_cancellation_and_unavailability_remain_distinct() {
    let cancelled_fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let cancelled = Arc::new(DetailedApprovalEmitter::new(
        ToolApprovalOutcome::Cancelled {
            reason: "approval client disconnected".to_string(),
        },
    ));
    let unavailable_fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let unavailable = Arc::new(DetailedApprovalEmitter::new(
        ToolApprovalOutcome::Unavailable,
    ));

    let cancelled_result = cancelled_fixture.handle(workflow_call(cancelled)).await;
    let unavailable_result = unavailable_fixture.handle(workflow_call(unavailable)).await;

    let Err(cancelled_error) = cancelled_result else {
        panic!("cancelled approval should prevent launch");
    };
    let Err(unavailable_error) = unavailable_result else {
        panic!("unavailable approval should prevent launch");
    };
    assert_eq!(
        cancelled_error,
        FunctionCallError::RespondToModel(
            "dynamic workflow approval was cancelled: approval client disconnected".to_string()
        )
    );
    assert_eq!(
        unavailable_error,
        FunctionCallError::RespondToModel(
            "dynamic workflow approval is required but unavailable in this client".to_string()
        )
    );
}

#[tokio::test]
async fn invalid_agent_prompt_is_rejected_before_approval_or_launch() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Approved));
    let source = "export const meta = { name: 'invalid-prompt', description: 'invalid prompt' };\nreturn agent(['review this', 'carefully']);";

    let result = fixture
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
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn never_approval_policy_launches_without_prompting() {
    let fixture = ToolFixture::new(AskForApproval::Never).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Denied));

    let output = fixture
        .handle(workflow_call(emitter.clone()))
        .await
        .unwrap();

    assert!(emitter.requests().is_empty());
    assert_eq!(
        output.code_mode_result(&workflow_payload())["status"],
        "async_launched"
    );
    assert_eq!(
        fixture.service.list(fixture.thread_id).await.unwrap().len(),
        1
    );
    fixture.wait_for_terminal().await;
}

#[tokio::test]
async fn strict_automatic_review_overrides_never_approval_policy() {
    let fixture = ToolFixture::new(AskForApproval::Never).await;
    let source = format!(
        "export const meta = {{ name: 'strict-large', description: 'strict artifact review' }};\n/* {} */\nreturn 'reviewed';",
        "x".repeat(9_000)
    );
    let emitter = Arc::new(ArtifactReviewingApprovalEmitter::new(
        fixture.config.codex_home.clone(),
        fixture.thread_id,
        source.clone(),
    ));

    let output = fixture
        .handle(workflow_call_with_payload(
            emitter.clone(),
            workflow_payload_with_source(&source),
        ))
        .await
        .unwrap();

    assert!(emitter.reviewed());
    let requests = emitter.requests();
    let [request] = requests.as_slice() else {
        panic!("expected one strict structured approval request");
    };
    let artifact = request.artifact.as_ref().expect("approval artifact");
    assert!(artifact.has_valid_sha256());
    assert!(emitter.user_requests().is_empty());
    assert_eq!(
        output.code_mode_result(&workflow_payload_with_source(&source))["status"],
        "async_launched"
    );
    fixture.wait_for_terminal().await;
}

#[tokio::test]
async fn non_strict_automatic_review_uses_artifact_for_large_action() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(DetailedApprovalEmitter::automatic(
        ToolApprovalReviewMode::Automatic,
        ToolApprovalOutcome::Approved,
        ToolApprovalOutcome::Unavailable,
    ));
    let child_source = "export const meta = { name: 'large-review-child', description: 'frozen approval child' }; return 'child-result';";
    let child_path = fixture
        .config
        .cwd
        .join(".codex/workflows/large-review-child.js");
    tokio::fs::create_dir_all(child_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&child_path, child_source).await.unwrap();
    let source = format!(
        "export const meta = {{ name: 'large-review', description: 'large review action' }};\n/* {} */\nreturn workflow('large-review-child');",
        "x".repeat(9_000)
    );

    let output = fixture
        .handle(workflow_call_with_payload(
            emitter.clone(),
            workflow_payload_with_source(&source),
        ))
        .await
        .unwrap();

    let requests = emitter.requests();
    let [request] = requests.as_slice() else {
        panic!("expected one structured approval request");
    };
    let review_artifact = request.artifact.as_ref().expect("approval artifact");
    assert!(review_artifact.has_valid_sha256());
    let canonical_action = canonical_json_value(request.action.clone());
    let action_text = serde_json::to_string_pretty(&canonical_action).unwrap();
    let action_sha256 = format!("{:x}", Sha256::digest(action_text.as_bytes()));
    let artifact_path = workflow_session_dir(&fixture.config.codex_home, fixture.thread_id)
        .join("workflows/approvals")
        .join(format!("{action_sha256}.json"));
    let persisted_action: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&artifact_path).await.unwrap()).unwrap();
    assert_eq!(persisted_action, canonical_action);
    assert_eq!(review_artifact.contents(), action_text);
    assert_eq!(persisted_action["reviewedScript"]["source"], source);
    assert_eq!(
        persisted_action["reviewedChildren"][0]["binding"],
        json!({ "kind": "name", "name": "large-review-child" })
    );
    assert_eq!(
        persisted_action["reviewedChildren"][0]["reviewedScript"]["source"],
        child_source
    );
    assert!(emitter.user_requests().is_empty());
    assert_eq!(
        output.code_mode_result(&workflow_payload_with_source(&source))["status"],
        "async_launched"
    );
    fixture.wait_for_terminal().await;
}

#[tokio::test]
async fn automatic_review_preserves_configuration_denial() {
    let fixture = ToolFixture::new(AskForApproval::OnRequest).await;
    let emitter = Arc::new(DetailedApprovalEmitter::automatic(
        ToolApprovalReviewMode::Automatic,
        ToolApprovalOutcome::Denied {
            rejection: "denied by policy hook".to_string(),
            source: ToolApprovalDenialSource::Configuration,
        },
        ToolApprovalOutcome::Approved,
    ));

    let result = fixture.handle(workflow_call(emitter.clone())).await;

    assert_eq!(
        result.err(),
        Some(FunctionCallError::RespondToModel(
            "configuration denied the dynamic workflow: denied by policy hook".to_string()
        ))
    );
    assert_eq!(emitter.requests().len(), 1);
    assert!(emitter.user_requests().is_empty());
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
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
    config: Config,
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
            service.clone(),
            AgentRunner::new(Weak::<ThreadManager>::new()),
            Weak::<ThreadManager>::new(),
        );
        Self {
            _codex_home: codex_home,
            config,
            thread_id,
            service,
            executor,
        }
    }

    async fn handle(&self, invocation: ToolCall) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let input =
            serde_json::from_str(invocation.function_arguments()?).map_err(model_bounded_error)?;
        self.executor
            .handle_with_context(
                invocation,
                input,
                WorkflowExecutionContext {
                    config: self.config.clone(),
                    environments: Vec::new(),
                    tool_environments: Vec::new(),
                    captured_environments: None,
                    execution_environment_action: json!({}),
                    location: WorkflowEnvironmentLocation::Local,
                    script_access: WorkflowScriptAccess::HostFilesystem,
                },
            )
            .await
    }

    async fn wait_for_terminal(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let terminal = self
                    .service
                    .list(self.thread_id)
                    .await
                    .unwrap()
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

fn local_workflow_execution_context(
    config: &codex_core::config::Config,
) -> WorkflowExecutionContext {
    let cwd = PathUri::from_abs_path(&config.cwd);
    let selection = TurnEnvironmentSelection {
        environment_id: "local".to_string(),
        cwd: cwd.clone(),
        workspace_roots: vec![cwd.clone()],
        config: EnvironmentConfigState::FromThread,
    };
    let sandbox = FileSystemSandboxContext::from_permission_profile_with_cwd(
        PermissionProfile::Disabled,
        cwd.clone(),
    );
    let tool_environment = ToolExecutionEnvironment::new(
        "local".to_string(),
        cwd,
        Some(selection.clone()),
        /*is_remote*/ false,
        "local-executor".to_string(),
        Arc::clone(&codex_exec_server::LOCAL_FS),
        sandbox,
        Arc::new(()),
    );
    WorkflowExecutionContext {
        config: config.clone(),
        environments: vec![selection],
        tool_environments: vec![tool_environment],
        captured_environments: None,
        execution_environment_action: json!({}),
        location: WorkflowEnvironmentLocation::Local,
        script_access: WorkflowScriptAccess::HostFilesystem,
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
        source: ToolCallSource::Direct,
        conversation_history: ConversationHistory::default(),
        turn_item_emitter: emitter,
        environments: Vec::new(),
        agent_configuration: None,
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

#[tokio::test]
async fn owning_sampling_step_agent_configuration_is_used_verbatim() {
    let codex_home = tempfile::tempdir().unwrap();
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    config.model = Some("effective-step-model".to_string());
    config.model_reasoning_effort = Some(codex_protocol::openai_models::ReasoningEffort::High);
    config.service_tier = Some("priority".to_string());
    config.developer_instructions = Some("effective developer instructions".to_string());
    let mut invocation = workflow_call(Arc::new(ApprovalEmitter::new(
        ToolApprovalDecision::Approved,
    )));
    invocation.agent_configuration = Some(codex_extension_api::ToolAgentConfiguration::new(
        config.clone(),
    ));

    assert_eq!(projected_agent_config(&invocation).unwrap(), config);
}

#[test]
fn missing_owning_sampling_step_agent_configuration_fails_closed() {
    let invocation = workflow_call(Arc::new(ApprovalEmitter::new(
        ToolApprovalDecision::Approved,
    )));

    let error = projected_agent_config(&invocation).unwrap_err();

    assert_eq!(
        error,
        FunctionCallError::RespondToModel(
            "Workflow cannot establish an authoritative execution context because the owning sampling step did not expose its effective agent configuration"
                .to_string()
        )
    );
}

#[test]
fn foreign_environment_paths_do_not_fall_back_to_host_filesystem() {
    #[cfg(unix)]
    let foreign_cwd = "file:///C:/workspace";
    #[cfg(windows)]
    let foreign_cwd = "file:///workspace";
    let foreign_cwd = PathUri::parse(foreign_cwd).unwrap();
    let environment = TurnEnvironmentSelection {
        environment_id: "remote".to_string(),
        cwd: foreign_cwd.clone(),
        workspace_roots: vec![foreign_cwd],
        config: codex_protocol::protocol::EnvironmentConfigState::Pending,
    };

    assert_eq!(host_native_workflow_paths(&environment).unwrap(), None);
}

#[test]
fn same_os_remote_environment_does_not_fall_back_to_host_filesystem() {
    let cwd = tempfile::tempdir().unwrap();
    let cwd =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(cwd.path().to_path_buf()).unwrap();
    let cwd = PathUri::from_abs_path(&cwd);
    let environment = TurnEnvironmentSelection {
        environment_id: "same-os-remote".to_string(),
        cwd: cwd.clone(),
        workspace_roots: vec![cwd],
        config: codex_protocol::protocol::EnvironmentConfigState::Pending,
    };

    assert_eq!(
        workflow_host_paths(WorkflowEnvironmentLocation::Remote, &environment).unwrap(),
        None
    );
}

#[test]
fn workflow_launch_uses_captured_environment_selections_after_live_selection_drift() {
    let primary = tempfile::tempdir().unwrap();
    let secondary = tempfile::tempdir().unwrap();
    let redirected = tempfile::tempdir().unwrap();
    let primary_cwd =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(primary.path().to_path_buf()).unwrap();
    let primary_cwd = PathUri::from_abs_path(&primary_cwd);
    let secondary_cwd =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(secondary.path().to_path_buf())
            .unwrap();
    let secondary_cwd = PathUri::from_abs_path(&secondary_cwd);
    let captured_config = codex_protocol::protocol::EnvironmentConfig {
        allow_login_shell: false,
        workspace_roots: Vec::new(),
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: true,
        use_legacy_landlock: false,
        permission_profile: codex_protocol::models::PermissionProfileSnapshot::legacy(
            codex_protocol::models::PermissionProfile::read_only(),
        ),
        shell_environment_policy: Default::default(),
        exec_policy: None,
        mcp_policy: None,
        network_policy: None,
        selected_capability_roots: Vec::new(),
    };
    let captured = vec![
        TurnEnvironmentSelection {
            environment_id: "primary".to_string(),
            cwd: primary_cwd.clone(),
            workspace_roots: vec![primary_cwd],
            config: codex_protocol::protocol::EnvironmentConfigState::Ready(
                captured_config.clone(),
            ),
        },
        TurnEnvironmentSelection {
            environment_id: "secondary".to_string(),
            cwd: secondary_cwd.clone(),
            workspace_roots: vec![secondary_cwd],
            config: codex_protocol::protocol::EnvironmentConfigState::Ready(
                captured_config.clone(),
            ),
        },
    ];
    let mut live = captured.clone();
    let redirected_root =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(redirected.path().to_path_buf())
            .unwrap();
    let redirected_root = PathUri::from_abs_path(&redirected_root);
    live[0].workspace_roots = vec![redirected_root.clone()];
    live[0].config = codex_protocol::protocol::EnvironmentConfigState::Ready(
        codex_protocol::protocol::EnvironmentConfig {
            allow_login_shell: true,
            ..captured_config
        },
    );
    live[1] = TurnEnvironmentSelection {
        environment_id: "redirected-secondary".to_string(),
        cwd: redirected_root.clone(),
        workspace_roots: vec![redirected_root],
        config: codex_protocol::protocol::EnvironmentConfigState::Pending,
    };

    let launch_selections = captured_environment_selections(captured.iter().map(|selection| {
        (
            selection.environment_id.as_str(),
            &selection.cwd,
            Some(selection),
        )
    }))
    .unwrap();

    assert_eq!(launch_selections, captured);
    assert_ne!(launch_selections, live);
}

#[test]
fn inline_only_access_rejects_saved_and_mixed_sources() {
    let inline: WorkflowInput = serde_json::from_value(json!({ "script": "return null" })).unwrap();
    assert!(validate_script_access(&inline, WorkflowScriptAccess::InlineOnly).is_ok());

    for input in [
        json!({ "name": "saved" }),
        json!({ "scriptPath": "saved.js" }),
        json!({ "script": "return null", "name": "saved" }),
        json!({ "script": "return null", "scriptPath": "saved.js" }),
    ] {
        let input = serde_json::from_value(input).unwrap();
        let Err(FunctionCallError::RespondToModel(message)) =
            validate_script_access(&input, WorkflowScriptAccess::InlineOnly)
        else {
            panic!("foreign environments must reject saved Workflow sources");
        };
        assert!(message.contains("accepts only the `script` source"));
    }
}

#[test]
fn model_launch_response_preserves_preflighted_paths_and_identifiers() {
    let launch = WorkflowLaunch {
        status: "async_launched".to_string(),
        task_id: "w12345678".to_string(),
        task_type: "local_workflow".to_string(),
        workflow_name: "Release Review".to_string(),
        run_id: "wf_abc123".to_string(),
        summary: "Running workflow Release Review".to_string(),
        transcript_dir: format!("/{}", "host-prefix/".repeat(1_000)),
        script_path: format!("/{}script.js", "host-prefix/".repeat(1_000)),
    };

    let response = model_launch_response(&launch);

    assert_eq!(response["runId"], "wf_abc123");
    assert_eq!(response["taskId"], "w12345678");
    assert_eq!(response["transcriptDir"], launch.transcript_dir);
    assert_eq!(response["scriptPath"], launch.script_path);
    let error = model_bounded_json_value(WORKFLOW_TOOL_NAME, &response)
        .expect_err("oversized paths must be rejected during launch preflight");
    assert!(
        error
            .to_string()
            .contains("should return a focused response")
    );
}

#[tokio::test]
async fn oversized_launch_paths_fail_before_workflow_is_registered() {
    let mut fixture = ToolFixture::new(AskForApproval::Never).await;
    fixture.config.codex_home = codex_utils_absolute_path::AbsolutePathBuf::try_from(
        fixture._codex_home.path().join("x".repeat(4_000)),
    )
    .unwrap();

    let result = fixture
        .handle(workflow_call(Arc::new(ApprovalEmitter::new(
            ToolApprovalDecision::Approved,
        ))))
        .await;

    let Err(error) = result else {
        panic!("oversized launch response should fail before launch");
    };
    assert!(
        error
            .to_string()
            .contains("should return a focused response")
    );
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unavailable_thread_manager_fails_closed_before_approval_or_launch() {
    let fixture = ToolFixture::new(AskForApproval::Never).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Approved));

    let result = fixture
        .executor
        .handle(workflow_call(emitter.clone()))
        .await;

    let Err(error) = result else {
        panic!("missing owning thread manager should fail closed");
    };
    assert_eq!(
        error,
        FunctionCallError::RespondToModel(
            "Workflow cannot establish an authoritative execution context because the owning thread manager is unavailable"
                .to_string()
        )
    );
    assert!(emitter.requests().is_empty());
    assert!(
        fixture
            .service
            .list(fixture.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn main_workflow_parse_and_resolution_errors_are_model_bounded() {
    let fixture = ToolFixture::new(AskForApproval::Never).await;
    let emitter = Arc::new(ApprovalEmitter::new(ToolApprovalDecision::Approved));
    let unknown_field = "x".repeat(10_000);
    let parse_call = workflow_call_with_payload(
        emitter.clone(),
        ToolPayload::Function {
            arguments: format!(r#"{{"{unknown_field}":true}}"#),
        },
    );
    let Err(FunctionCallError::RespondToModel(parse_error)) =
        fixture.executor.handle(parse_call).await
    else {
        panic!("invalid input should be rejected before context lookup");
    };
    assert!(parse_error.len() <= crate::workflow_result_tool::MODEL_ERROR_MAX_BYTES);

    let result = fixture
        .handle(workflow_call_with_payload(
            emitter,
            ToolPayload::Function {
                arguments: json!({ "name": "n".repeat(10_000) }).to_string(),
            },
        ))
        .await;
    let Err(FunctionCallError::RespondToModel(resolve_error)) = result else {
        panic!("invalid saved Workflow name should be rejected");
    };
    assert!(resolve_error.len() <= crate::workflow_result_tool::MODEL_ERROR_MAX_BYTES);
}

#[test]
fn execution_context_diagnostics_are_bounded_before_reaching_the_model() {
    let environment_id = "environment".repeat(2_000);
    let selected_cwd = "selected".repeat(2_000);
    let tool_cwd = "tool".repeat(2_000);

    let FunctionCallError::RespondToModel(message) = workflow_diagnostic(format_args!(
        "Workflow execution context mismatch for environment `{environment_id}`: selected cwd is {selected_cwd}, but the tool filesystem cwd is {tool_cwd}"
    )) else {
        panic!("execution context diagnostics should be model-visible");
    };

    assert!(message.len() <= crate::workflow_result_tool::MODEL_ERROR_MAX_BYTES);
    assert!(message.ends_with("...[truncated]"));
}
