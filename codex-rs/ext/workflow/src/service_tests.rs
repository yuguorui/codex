use super::*;
use codex_config::LoaderOverrides;
use codex_core::ThreadManager;
use codex_core::config::ConfigBuilder;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionWarning;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::mpsc;

const WORKFLOW_SOURCE: &str = "export const meta = { name: 'restore-test', description: 'restore a persisted run' }; return 'restored'";

struct RecordingEventSink {
    sender: mpsc::UnboundedSender<Event>,
}

impl ExtensionEventSink for RecordingEventSink {
    fn emit(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    fn emit_warning(&self, _warning: ExtensionWarning) {}
}

struct RestoreFixture {
    _codex_home: tempfile::TempDir,
    config: Config,
    thread_id: ThreadId,
    service: WorkflowService,
    events: mpsc::UnboundedReceiver<Event>,
}

impl RestoreFixture {
    async fn new() -> Self {
        let codex_home = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await
            .unwrap();
        let thread_id = ThreadId::from_string("22222222-2222-4222-8222-222222222222").unwrap();
        let (sender, events) = mpsc::unbounded_channel();
        let service = WorkflowService::new(Arc::new(RecordingEventSink { sender }), Weak::new());
        Self {
            _codex_home: codex_home,
            config,
            thread_id,
            service,
            events,
        }
    }

    async fn persist(&self, status: WorkflowTaskStatus, source: &str) -> WorkflowTaskSnapshot {
        let run_id = "wf_restore-test";
        let session_dir = workflow_session_dir(&self.config.codex_home, self.thread_id);
        let scripts_dir = session_dir.join("workflows/scripts");
        let transcript_dir = session_dir.join("subagents/workflows").join(run_id);
        tokio::fs::create_dir_all(&scripts_dir).await.unwrap();
        tokio::fs::create_dir_all(&transcript_dir).await.unwrap();
        let script_path = scripts_dir.join("restore-test.js");
        tokio::fs::write(&script_path, source).await.unwrap();
        let snapshot = WorkflowTaskSnapshot {
            thread_id: self.thread_id.to_string(),
            turn_id: "turn-restore".to_string(),
            task_id: "wrestore1".to_string(),
            run_id: run_id.to_string(),
            workflow_name: "restore-test".to_string(),
            title: Some("Restore test".to_string()),
            status,
            summary: "Persisted workflow".to_string(),
            transcript_dir: transcript_dir.clone(),
            script_path: script_path.clone(),
            args: JsonValue::Null,
            result: JsonValue::Null,
            output_file: transcript_dir.join("wrestore1.output"),
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage::default(),
            failures: Vec::new(),
            error: None,
            started_at: 100,
            completed_at: matches!(
                status,
                WorkflowTaskStatus::Completed
                    | WorkflowTaskStatus::Failed
                    | WorkflowTaskStatus::Paused
                    | WorkflowTaskStatus::Killed
            )
            .then_some(200),
            script_sha256: sha256(WORKFLOW_SOURCE),
        };
        write_json(snapshot_path(&snapshot).unwrap(), &snapshot)
            .await
            .unwrap();
        snapshot
    }

    async fn restore(&self) {
        self.service
            .restore_thread(
                self.thread_id,
                self.config.clone(),
                AgentRunner::new(Weak::<ThreadManager>::new()),
                Vec::new(),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn restores_terminal_snapshot_into_thread_history() {
    let fixture = RestoreFixture::new().await;
    let mut snapshot = fixture
        .persist(WorkflowTaskStatus::Completed, WORKFLOW_SOURCE)
        .await;
    snapshot.output_file = snapshot_path(&snapshot).unwrap();

    fixture.restore().await;

    assert_eq!(fixture.service.list(fixture.thread_id), vec![snapshot]);
    assert!(
        !fixture
            .service
            .stop(fixture.thread_id, "wf_restore-test")
            .unwrap()
    );
}

#[tokio::test]
async fn pauses_active_snapshot_when_approved_script_changed() {
    let fixture = RestoreFixture::new().await;
    let mut expected = fixture
        .persist(
            WorkflowTaskStatus::Running,
            "export const meta = { name: 'changed', description: 'changed' }; return null",
        )
        .await;

    fixture.restore().await;

    expected.status = WorkflowTaskStatus::Paused;
    expected.summary = "Workflow restore-test paused".to_string();
    expected.error = Some(
        "script content changed since it was approved; resume via the Workflow tool to re-approve"
            .to_string(),
    );
    expected.output_file = snapshot_path(&expected).unwrap();
    assert_eq!(
        fixture.service.list(fixture.thread_id),
        vec![expected.clone()]
    );
    let persisted: WorkflowTaskSnapshot = serde_json::from_slice(
        &tokio::fs::read(snapshot_path(&expected).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(persisted, expected);
}

#[tokio::test]
async fn adopts_active_snapshot_and_completes_it_when_script_matches() {
    let mut fixture = RestoreFixture::new().await;
    fixture
        .persist(WorkflowTaskStatus::Running, WORKFLOW_SOURCE)
        .await;

    fixture.restore().await;

    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = fixture.events.recv().await.expect("workflow event channel");
            if let EventMsg::WorkflowCompleted(completed) = event.msg {
                break completed;
            }
        }
    })
    .await
    .expect("restored workflow should complete");
    assert_eq!(completed.status, WorkflowTaskStatus::Completed);
    assert_eq!(completed.run_id, "wf_restore-test");
    let snapshots = fixture.service.list(fixture.thread_id);
    let [snapshot] = snapshots.as_slice() else {
        panic!("restored workflow should remain in service history");
    };
    assert_eq!(snapshot.status, WorkflowTaskStatus::Completed);
    assert_eq!(snapshot.summary, "Workflow restore-test completed");
    assert_eq!(snapshot.result, json!("restored"));
    assert_eq!(completed.output_file, snapshot_path(snapshot).unwrap());
    assert_eq!(snapshot.output_file, completed.output_file);
    assert!(
        tokio::fs::try_exists(snapshot_path(snapshot).unwrap())
            .await
            .unwrap()
    );
    let persisted: WorkflowTaskSnapshot =
        serde_json::from_slice(&tokio::fs::read(&snapshot.output_file).await.unwrap()).unwrap();
    assert_eq!(&persisted, snapshot);
}

#[tokio::test]
async fn explicit_resume_reuses_the_run_id_after_an_edited_script_is_reapproved() {
    let mut fixture = RestoreFixture::new().await;
    let previous = fixture
        .persist(WorkflowTaskStatus::Completed, WORKFLOW_SOURCE)
        .await;
    fixture.restore().await;
    let edited_source = "export const meta = { name: 'restore-test', description: 'edited and reapproved' }; return 'edited result'";
    let edited_script = validate_workflow_script(edited_source).unwrap();

    let launch = fixture
        .service
        .launch(WorkflowLaunchRequest {
            thread_id: fixture.thread_id,
            turn_id: "turn-resume".to_string(),
            config: fixture.config.clone(),
            resolved: ResolvedWorkflow {
                script: edited_script,
                args: json!({ "revision": 2 }),
                resume_from_run_id: Some(previous.run_id.clone()),
                origin: crate::discovery::WorkflowOrigin::Inline,
                shadows_existing: false,
            },
            agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
            token_budget: None,
            plugin_roots: Vec::new(),
        })
        .await
        .unwrap();

    assert_eq!(launch.run_id, previous.run_id);
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = fixture.events.recv().await.expect("workflow event channel");
            if let EventMsg::WorkflowCompleted(completed) = event.msg {
                break completed;
            }
        }
    })
    .await
    .expect("resumed workflow should complete");
    assert_eq!(completed.run_id, launch.run_id);
    assert_eq!(completed.status, WorkflowTaskStatus::Completed);
    let snapshot = fixture.service.list(fixture.thread_id).remove(0);
    assert_eq!(snapshot.result, json!("edited result"));
    assert_eq!(snapshot.script_sha256, sha256(edited_source));
}

#[tokio::test]
async fn terminal_agent_failure_completes_with_null_and_preserves_diagnostics() {
    let mut fixture = RestoreFixture::new().await;
    let script = validate_workflow_script(
        "export const meta = { name: 'failure-test', description: 'record failure' }; return agent('fail')",
    )
    .unwrap();
    let launch = fixture
        .service
        .launch(WorkflowLaunchRequest {
            thread_id: fixture.thread_id,
            turn_id: "turn-failure".to_string(),
            config: fixture.config.clone(),
            resolved: ResolvedWorkflow {
                script,
                args: JsonValue::Null,
                resume_from_run_id: None,
                origin: crate::discovery::WorkflowOrigin::Inline,
                shadows_existing: false,
            },
            agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
            token_budget: None,
            plugin_roots: Vec::new(),
        })
        .await
        .unwrap();

    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = fixture.events.recv().await.expect("workflow event channel");
            if let EventMsg::WorkflowCompleted(completed) = event.msg {
                break completed;
            }
        }
    })
    .await
    .expect("failed workflow should publish a terminal event");

    assert_eq!(completed.status, WorkflowTaskStatus::Completed);
    assert_eq!(completed.usage.agent_count, 1);
    assert_eq!(completed.failures.len(), 1);
    assert!(completed.failures[0].contains("thread manager dropped"));
    let snapshot = fixture
        .service
        .list(fixture.thread_id)
        .into_iter()
        .find(|snapshot| snapshot.run_id == launch.run_id)
        .expect("completed workflow snapshot");
    assert_eq!(snapshot.result, JsonValue::Null);
    assert_eq!(snapshot.failures, completed.failures);
    assert_eq!(snapshot.output_file, snapshot_path(&snapshot).unwrap());
    let persisted: WorkflowTaskSnapshot =
        serde_json::from_slice(&tokio::fs::read(&snapshot.output_file).await.unwrap()).unwrap();
    assert_eq!(persisted, snapshot);
    let mut transcript_entries = tokio::fs::read_dir(&snapshot.transcript_dir).await.unwrap();
    while let Some(entry) = transcript_entries.next_entry().await.unwrap() {
        assert_ne!(
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str()),
            Some("output")
        );
    }
}

#[test]
fn task_cache_keeps_active_tasks_and_only_the_newest_terminal_tasks() {
    let root = tempfile::tempdir().unwrap();
    let mut tasks = HashMap::new();
    for (run_id, started_at, status) in [
        ("wf_old", 1, WorkflowTaskStatus::Completed),
        ("wf_middle", 2, WorkflowTaskStatus::Failed),
        ("wf_new", 3, WorkflowTaskStatus::Killed),
        ("wf_active", 0, WorkflowTaskStatus::Running),
    ] {
        tasks.insert(
            run_id.to_string(),
            workflow_task(root.path(), run_id, started_at, status),
        );
    }

    prune_terminal_tasks(&mut tasks, 2);

    let mut retained = tasks.keys().cloned().collect::<Vec<_>>();
    retained.sort();
    assert_eq!(
        retained,
        vec![
            "wf_active".to_string(),
            "wf_middle".to_string(),
            "wf_new".to_string(),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn progress_persistence_writes_at_most_once_per_snapshot_interval() {
    let root = tempfile::tempdir().unwrap();
    let task = workflow_task(root.path(), "wf_debounce", 1, WorkflowTaskStatus::Running);
    let initial = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let path = snapshot_path(&initial).unwrap();
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    write_json(&path, &initial).await.unwrap();

    for version in 1..=3 {
        task.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .progress_version = version;
        persist_task_background(Arc::clone(&task));
    }
    tokio::task::yield_now().await;
    tokio::time::advance(SNAPSHOT_PERSIST_INTERVAL - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let before: WorkflowTaskSnapshot =
        serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    assert_eq!(before.progress_version, 0);

    tokio::time::advance(Duration::from_millis(1)).await;
    for _ in 0..10_000 {
        if !task
            .persist_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        !task
            .persist_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running,
        "debounced persistence worker did not finish"
    );
    let after: WorkflowTaskSnapshot =
        serde_json::from_slice(&tokio::fs::read(path).await.unwrap()).unwrap();
    assert_eq!(after.progress_version, 3);
}

fn workflow_task(
    root: &std::path::Path,
    run_id: &str,
    started_at: i64,
    status: WorkflowTaskStatus,
) -> Arc<WorkflowTask> {
    let root = AbsolutePathBuf::try_from(root.to_path_buf()).unwrap();
    let script_path = root.join("workflows/scripts").join(format!("{run_id}.js"));
    let transcript_dir = root.join("transcripts").join(run_id);
    let output_file = root.join("workflows").join(format!("{run_id}.json"));
    Arc::new(WorkflowTask {
        snapshot: Mutex::new(WorkflowTaskSnapshot {
            thread_id: "test-thread".to_string(),
            turn_id: "test-turn".to_string(),
            task_id: format!("task-{run_id}"),
            run_id: run_id.to_string(),
            workflow_name: "test".to_string(),
            title: None,
            status,
            summary: "test".to_string(),
            transcript_dir,
            script_path,
            args: JsonValue::Null,
            result: JsonValue::Null,
            output_file,
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage::default(),
            failures: Vec::new(),
            error: None,
            started_at,
            completed_at: None,
            script_sha256: "test".to_string(),
        }),
        persist_lock: Semaphore::new(1),
        persist_state: Mutex::new(PersistState::default()),
        control: WorkflowControl::new(),
    })
}
