use super::*;
use crate::persistence::snapshot_path;
use crate::persistence::workflow_session_dir;
use crate::persistence::write_json;
use crate::service::WorkflowTaskSnapshot;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_extension_api::ExtensionData;
use codex_extension_api::NoopExtensionEventSink;
use codex_protocol::workflow::WorkflowTaskStatus;
use codex_protocol::workflow::WorkflowUsage;
use pretty_assertions::assert_eq;
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
    config.features.disable(Feature::Workflows);
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
        result: serde_json::Value::Null,
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

    assert_eq!(service.list(thread_id), Vec::new());
    assert!(
        !thread_store
            .get::<WorkflowThreadConfig>()
            .expect("workflow thread config")
            .enabled
    );
    assert!(extension.tools(&session_store, &thread_store).is_empty());
}
