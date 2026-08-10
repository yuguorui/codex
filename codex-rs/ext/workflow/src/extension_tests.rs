use super::*;
use crate::analyze_inputs::ANALYZE_WORKFLOW_INPUTS_TOOL_NAME;
use crate::persistence::snapshot_path;
use crate::persistence::workflow_session_dir;
use crate::persistence::write_json;
use crate::service::WorkflowTaskSnapshot;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_extension_api::ExtensionData;
use codex_extension_api::NoopExtensionEventSink;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolName;
use codex_extension_api::ToolPayload;
use codex_protocol::protocol::SessionSource;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_protocol::workflow::WorkflowUsage;
use codex_utils_output_truncation::TruncationPolicy;
use codex_workflow::MemoryWorkflowInputArtifactStore;
use codex_workflow::WorkflowAgentInputs;
use codex_workflow::WorkflowInputArtifactStore;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Weak;

#[tokio::test]
async fn disabled_feature_does_not_restore_persisted_workflows() {
    let codex_home = tempfile::tempdir().unwrap();
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let _ = config.features.disable(Feature::Workflows);
    assert!(!config.features.enabled(Feature::Workflows));

    let thread_id = ThreadId::from_string("33333333-3333-4333-8333-333333333333").unwrap();
    let scripts_dir = workflow_session_dir(&config.codex_home, thread_id).join("workflows/scripts");
    tokio::fs::create_dir_all(&scripts_dir).await.unwrap();
    let script_path = scripts_dir.join("disabled-restore.js");
    let snapshot = WorkflowTaskSnapshot {
        thread_id: thread_id.to_string(),
        turn_id: "turn-disabled".to_string(),
        task_id: "wdisabled".to_string(),
        run_id: "wf_disabled-restore".to_string(),
        workflow_name: "disabled-restore".to_string(),
        title: None,
        status: WorkflowTaskStatus::Completed,
        summary: "Persisted while enabled".to_string(),
        transcript_dir: scripts_dir.clone(),
        script_path,
        args: serde_json::Value::Null,
        result_artifact: None,
        output_file: scripts_dir.join("wdisabled.output"),
        progress: Vec::new(),
        progress_version: 0,
        usage: WorkflowUsage::default(),
        failures: Vec::new(),
        error: None,
        started_at: 100,
        completed_at: Some(200),
        script_sha256: "unused-for-terminal-run".to_string(),
    };
    write_json(snapshot_path(&snapshot).unwrap(), &snapshot)
        .await
        .unwrap();

    let service = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
    let extension = WorkflowExtension {
        service: service.clone(),
        agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
        thread_manager: Weak::<ThreadManager>::new(),
    };
    let session_store = ExtensionData::new("session-disabled");
    let thread_store = ExtensionData::new(thread_id.to_string());
    let session_source = SessionSource::Cli;

    extension
        .on_thread_start(ThreadStartInput {
            config: &config,
            session_source: &session_source,
            persistent_thread_state_available: true,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;

    assert_eq!(service.list(thread_id).await.unwrap(), Vec::new());
    assert!(
        !thread_store
            .get::<WorkflowThreadConfig>()
            .expect("workflow thread config")
            .enabled
    );
    assert!(extension.tools(&session_store, &thread_store).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_inputs_register_the_analysis_tool_and_rebuild_multiple_artifacts() {
    let codex_home = tempfile::tempdir().unwrap();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let store: Arc<dyn WorkflowInputArtifactStore> =
        Arc::new(MemoryWorkflowInputArtifactStore::default());
    let reports = store
        .put(json!([{"area": "core", "score": 7}, {"area": "tui", "score": 3}]))
        .await
        .unwrap();
    let claims = store
        .put(json!([{"area": "protocol", "score": 11}]))
        .await
        .unwrap();
    let init = crate::agent::workflow_agent_extension_init(Some(WorkflowAgentInputs::new(
        BTreeMap::from([
            ("reports".to_string(), reports),
            ("claims".to_string(), claims),
        ]),
        store,
    )));
    let thread_id = ThreadId::from_string("44444444-4444-4444-8444-444444444444").unwrap();
    let session_store = ExtensionData::new("session-analysis");
    let thread_store = ExtensionData::new_with_init(thread_id.to_string(), init);
    let extension = WorkflowExtension {
        service: WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new()),
        agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
        thread_manager: Weak::<ThreadManager>::new(),
    };
    extension
        .on_thread_start(ThreadStartInput {
            config: &config,
            session_source: &SessionSource::Cli,
            persistent_thread_state_available: false,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;
    let tools = extension.tools(&session_store, &thread_store);
    let [tool] = tools.as_slice() else {
        panic!("workflow agent should receive one analysis tool");
    };
    assert_eq!(
        tool.tool_name(),
        ToolName::plain(ANALYZE_WORKFLOW_INPUTS_TOOL_NAME)
    );
    let call = ToolCall {
        turn_id: "turn-analysis".to_string(),
        call_id: "call-analysis".to_string(),
        tool_name: ToolName::plain(ANALYZE_WORKFLOW_INPUTS_TOOL_NAME),
        model: "gpt-test".to_string(),
        codex_turn_metadata: None,
        truncation_policy: TruncationPolicy::Bytes(1024),
        conversation_history: codex_extension_api::ConversationHistory::default(),
        turn_item_emitter: Arc::new(codex_extension_api::NoopTurnItemEmitter),
        environments: Vec::new(),
        agent_configuration: None,
        payload: ToolPayload::Function {
            arguments: json!({
                "program": "return { aliases: Object.keys(inputs).sort(), total: [...inputs.reports, ...inputs.claims].reduce((sum, item) => sum + item.score, 0) };"
            })
            .to_string(),
        },
    };
    let payload = call.payload.clone();
    let output = tool.handle(call).await.unwrap();
    assert_eq!(
        output.code_mode_result(&payload),
        json!({
            "result": {"aliases": ["claims", "reports"], "total": 21},
            "logs": [],
            "logsTruncated": false,
        })
    );
}
