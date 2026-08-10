use super::*;
use codex_config::Constrained;
use codex_config::LoaderOverrides;
use codex_core::ThreadManager;
use codex_core::TurnInputRequest;
use codex_core::config::AgentRoleConfig;
use codex_core::config::ConfigBuilder;
use codex_extension_api::ExtensionEventAvailabilityFuture;
use codex_extension_api::ExtensionEventDeliveryFuture;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionWarning;
use codex_extension_api::NoopExtensionEventSink;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::process::Command;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::sync::mpsc;

const WORKFLOW_SOURCE: &str = "export const meta = { name: 'restore-test', description: 'restore a persisted run' }; return 'restored'";

async fn read_snapshot_result(snapshot: &WorkflowTaskSnapshot) -> JsonValue {
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

struct RecordingEventSink {
    sender: mpsc::UnboundedSender<Event>,
}

impl ExtensionEventSink for RecordingEventSink {
    fn emit(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    fn emit_warning(&self, _warning: ExtensionWarning) {}
}

#[derive(Default)]
struct BlockingCompletionEventSink {
    delivery_started: Arc<Notify>,
    release_delivery: Arc<Notify>,
}

struct RetryThenAcknowledgeEventSink {
    attempts: AtomicUsize,
}

struct AlwaysRetryEventSink {
    attempts: AtomicUsize,
}

struct AcknowledgeEventSink {
    attempts: AtomicUsize,
}

struct ReconnectingEventSink {
    attempts: AtomicUsize,
    connected: AtomicBool,
    waiting_for_availability: Notify,
    available: Notify,
    acknowledged: Notify,
}

fn event_idempotency_key(event: &Event) -> String {
    match &event.msg {
        EventMsg::WorkflowStarted(started) => format!(
            "workflow/started/{}/{}/{}",
            started.thread_id, started.run_id, started.task_id
        ),
        EventMsg::WorkflowCompleted(completed) => format!(
            "workflow/completed/{}/{}/{}",
            completed.thread_id, completed.run_id, completed.task_id
        ),
        _ => event.id.clone(),
    }
}

impl ExtensionEventSink for RetryThenAcknowledgeEventSink {
    fn emit(&self, _event: Event) {}

    fn emit_and_wait(&self, event: Event) -> ExtensionEventDeliveryFuture<'_> {
        let idempotency_key = event_idempotency_key(&event);
        let attempt = self.attempts.fetch_add(1, Ordering::AcqRel);
        Box::pin(std::future::ready(if attempt == 0 {
            codex_extension_api::ExtensionEventDelivery::Retryable { idempotency_key }
        } else {
            codex_extension_api::ExtensionEventDelivery::Acknowledged { idempotency_key }
        }))
    }

    fn emit_warning(&self, _warning: ExtensionWarning) {}
}

impl ExtensionEventSink for AlwaysRetryEventSink {
    fn emit(&self, _event: Event) {}

    fn emit_and_wait(&self, event: Event) -> ExtensionEventDeliveryFuture<'_> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        Box::pin(std::future::ready(
            codex_extension_api::ExtensionEventDelivery::Retryable {
                idempotency_key: event_idempotency_key(&event),
            },
        ))
    }

    fn emit_warning(&self, _warning: ExtensionWarning) {}
}

impl ExtensionEventSink for AcknowledgeEventSink {
    fn emit(&self, _event: Event) {}

    fn emit_and_wait(&self, event: Event) -> ExtensionEventDeliveryFuture<'_> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        Box::pin(std::future::ready(
            codex_extension_api::ExtensionEventDelivery::Acknowledged {
                idempotency_key: event_idempotency_key(&event),
            },
        ))
    }

    fn emit_warning(&self, _warning: ExtensionWarning) {}
}

impl ExtensionEventSink for ReconnectingEventSink {
    fn emit(&self, _event: Event) {}

    fn emit_and_wait(&self, event: Event) -> ExtensionEventDeliveryFuture<'_> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        let idempotency_key = event_idempotency_key(&event);
        if self.connected.load(Ordering::Acquire) {
            self.acknowledged.notify_one();
            Box::pin(std::future::ready(
                codex_extension_api::ExtensionEventDelivery::Acknowledged { idempotency_key },
            ))
        } else {
            Box::pin(std::future::ready(
                codex_extension_api::ExtensionEventDelivery::Retryable { idempotency_key },
            ))
        }
    }

    fn wait_for_delivery_availability(
        &self,
        _thread_id: ThreadId,
    ) -> Option<ExtensionEventAvailabilityFuture<'_>> {
        self.waiting_for_availability.notify_one();
        Some(Box::pin(self.available.notified()))
    }

    fn emit_warning(&self, _warning: ExtensionWarning) {}
}

impl ExtensionEventSink for BlockingCompletionEventSink {
    fn emit(&self, _event: Event) {}

    fn emit_and_wait(&self, event: Event) -> ExtensionEventDeliveryFuture<'_> {
        let idempotency_key = event_idempotency_key(&event);
        if !matches!(event.msg, EventMsg::WorkflowCompleted(_)) {
            return Box::pin(std::future::ready(
                codex_extension_api::ExtensionEventDelivery::Acknowledged { idempotency_key },
            ));
        }
        let delivery_started = Arc::clone(&self.delivery_started);
        let release_delivery = Arc::clone(&self.release_delivery);
        Box::pin(async move {
            delivery_started.notify_one();
            release_delivery.notified().await;
            codex_extension_api::ExtensionEventDelivery::Acknowledged { idempotency_key }
        })
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
        self.persist_named(
            "wf_restore-test",
            status,
            source,
            sha256(WORKFLOW_SOURCE),
            100,
        )
        .await
    }

    async fn persist_named(
        &self,
        run_id: &str,
        status: WorkflowTaskStatus,
        source: &str,
        script_sha256: String,
        started_at: i64,
    ) -> WorkflowTaskSnapshot {
        let mut snapshot = self
            .prepare_snapshot(run_id, status, source, script_sha256, started_at)
            .await;
        snapshot.output_file = snapshot_path(&snapshot).unwrap();
        let script = validate_workflow_script(source).unwrap();
        let environment = local_environment(&self.config);
        write_current_snapshot(
            &snapshot.output_file,
            &snapshot,
            &PersistedWorkflowExecutionContext::capture(
                &self.config,
                self.thread_id,
                WorkflowEnvironmentLocation::Local,
                &[environment],
            )
            .await,
            &PersistedWorkflowComposition::empty(&script),
        )
        .await
        .unwrap();
        snapshot
    }

    async fn prepare_snapshot(
        &self,
        run_id: &str,
        status: WorkflowTaskStatus,
        source: &str,
        script_sha256: String,
        started_at: i64,
    ) -> WorkflowTaskSnapshot {
        let session_dir = workflow_session_dir(&self.config.codex_home, self.thread_id);
        let scripts_dir = session_dir.join("workflows/scripts");
        let transcript_dir = session_dir.join("subagents/workflows").join(run_id);
        tokio::fs::create_dir_all(&scripts_dir).await.unwrap();
        tokio::fs::create_dir_all(&transcript_dir).await.unwrap();
        let script_path = scripts_dir.join(format!("{run_id}.js"));
        tokio::fs::write(&script_path, source).await.unwrap();

        WorkflowTaskSnapshot {
            thread_id: self.thread_id.to_string(),
            turn_id: "turn-restore".to_string(),
            task_id: format!("w{run_id}"),
            run_id: run_id.to_string(),
            workflow_name: "restore-test".to_string(),
            title: Some("Restore test".to_string()),
            status,
            summary: "Persisted workflow".to_string(),
            transcript_dir: transcript_dir.clone(),
            script_path: script_path.clone(),
            args: JsonValue::Null,
            result_artifact: None,
            output_file: transcript_dir.join(format!("{run_id}.output")),
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage::default(),
            failures: Vec::new(),
            error: None,
            started_at,
            completed_at: matches!(
                status,
                WorkflowTaskStatus::Completed
                    | WorkflowTaskStatus::Failed
                    | WorkflowTaskStatus::Paused
                    | WorkflowTaskStatus::Killed
            )
            .then_some(200),
            script_sha256,
        }
    }

    async fn persist_current(
        &self,
        status: WorkflowTaskStatus,
        source: &str,
        environment_location: WorkflowEnvironmentLocation,
        environments: &[TurnEnvironmentSelection],
    ) -> WorkflowTaskSnapshot {
        let mut snapshot = self
            .prepare_snapshot("wf_restore-test", status, source, sha256(source), 100)
            .await;
        snapshot.output_file = snapshot_path(&snapshot).unwrap();
        let script = validate_workflow_script(source).unwrap();
        let composition = PersistedWorkflowComposition::empty(&script);
        write_current_snapshot(
            &snapshot.output_file,
            &snapshot,
            &PersistedWorkflowExecutionContext::capture(
                &self.config,
                self.thread_id,
                environment_location,
                environments,
            )
            .await,
            &composition,
        )
        .await
        .unwrap();
        snapshot
    }

    async fn persist_current_with_frozen_child(
        &self,
        status: WorkflowTaskStatus,
        source: &str,
        child_source: &str,
    ) -> WorkflowTaskSnapshot {
        tokio::fs::write(self.config.cwd.join("child.js"), child_source)
            .await
            .unwrap();
        let script = validate_workflow_script(source).unwrap();
        let composition = crate::composition::freeze_workflow_composition(
            &script,
            crate::composition::ChildWorkflowPolicy::FreezeLocal,
            &self.config.cwd,
            &self.config.codex_home,
            &[],
        )
        .await
        .unwrap();
        let mut snapshot = self
            .prepare_snapshot("wf_restore-test", status, source, sha256(source), 100)
            .await;
        snapshot.output_file = snapshot_path(&snapshot).unwrap();
        let persisted_composition =
            persist_workflow_composition(&composition, &workflow_children_dir(&snapshot).unwrap())
                .await
                .unwrap();
        let environment = local_environment(&self.config);
        write_current_snapshot(
            &snapshot.output_file,
            &snapshot,
            &PersistedWorkflowExecutionContext::capture(
                &self.config,
                self.thread_id,
                WorkflowEnvironmentLocation::Local,
                &[environment],
            )
            .await,
            &persisted_composition,
        )
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
            )
            .await
            .unwrap();
    }

    async fn launch(
        &self,
        source: &str,
        resume_from_run_id: Option<String>,
        environments: Vec<TurnEnvironmentSelection>,
    ) -> WorkflowLaunch {
        self.try_launch(source, resume_from_run_id, environments)
            .await
            .unwrap()
    }

    async fn try_launch(
        &self,
        source: &str,
        resume_from_run_id: Option<String>,
        environments: Vec<TurnEnvironmentSelection>,
    ) -> Result<WorkflowLaunch, WorkflowServiceError> {
        self.try_launch_with_args(source, JsonValue::Null, resume_from_run_id, environments)
            .await
    }

    async fn try_launch_with_args(
        &self,
        source: &str,
        args: JsonValue,
        resume_from_run_id: Option<String>,
        environments: Vec<TurnEnvironmentSelection>,
    ) -> Result<WorkflowLaunch, WorkflowServiceError> {
        let script = validate_workflow_script(source).unwrap();
        let composition = FrozenWorkflowComposition::empty(&script);
        self.service
            .launch(WorkflowLaunchRequest {
                thread_id: self.thread_id,
                turn_id: "turn-launch".to_string(),
                config: self.config.clone(),
                resolved: ResolvedWorkflow {
                    script,
                    args,
                    resume_from_run_id,
                    origin: crate::discovery::WorkflowOrigin::Inline,
                    shadows_existing: false,
                    composition,
                },
                agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
                environments,
                captured_environments: None,
                environment_location: WorkflowEnvironmentLocation::Local,
            })
            .await
    }

    async fn launch_with_frozen_path_child(
        &self,
        source: &str,
        child_source: &str,
        resume_from_run_id: Option<String>,
        environments: Vec<TurnEnvironmentSelection>,
    ) -> WorkflowLaunch {
        tokio::fs::write(self.config.cwd.join("child.js"), child_source)
            .await
            .unwrap();
        let script = validate_workflow_script(source).unwrap();
        let composition = crate::composition::freeze_workflow_composition(
            &script,
            crate::composition::ChildWorkflowPolicy::FreezeLocal,
            &self.config.cwd,
            &self.config.codex_home,
            &[],
        )
        .await
        .unwrap();
        self.service
            .launch(WorkflowLaunchRequest {
                thread_id: self.thread_id,
                turn_id: "turn-child-launch".to_string(),
                config: self.config.clone(),
                resolved: ResolvedWorkflow {
                    script,
                    args: JsonValue::Null,
                    resume_from_run_id,
                    origin: crate::discovery::WorkflowOrigin::Inline,
                    shadows_existing: false,
                    composition,
                },
                agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
                environments,
                captured_environments: None,
                environment_location: WorkflowEnvironmentLocation::Local,
            })
            .await
            .unwrap()
    }

    async fn wait_for_completion(&mut self, run_id: &str) -> WorkflowCompletedEvent {
        let task_id = self
            .service
            .cached_task(self.thread_id, run_id)
            .expect("workflow should be cached while awaiting completion")
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .task_id
            .clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = self.events.recv().await.expect("workflow event channel");
                if let EventMsg::WorkflowCompleted(completed) = event.msg
                    && completed.run_id == run_id
                    && completed.task_id == task_id
                {
                    break completed;
                }
            }
        })
        .await
        .expect("workflow should complete")
    }
}

fn local_environment(config: &Config) -> TurnEnvironmentSelection {
    TurnEnvironmentSelection {
        environment_id: "local".to_string(),
        cwd: PathUri::from_abs_path(&config.cwd),
        workspace_roots: config
            .workspace_roots
            .iter()
            .map(PathUri::from_abs_path)
            .collect(),
        config: EnvironmentConfigState::FromThread,
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

    assert_eq!(
        fixture.service.list(fixture.thread_id).await.unwrap(),
        vec![snapshot]
    );
    assert!(
        !fixture
            .service
            .stop(fixture.thread_id, "wf_restore-test")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn evicted_terminal_history_is_lazy_loaded_after_restart() {
    let fixture = RestoreFixture::new().await;
    let history_len = MAX_RETAINED_TERMINAL_TASKS + 3;
    let oldest_run_id = "wf_history-000";
    let resumable_run_id = "wf_history-001";
    for index in 0..history_len {
        let run_id = format!("wf_history-{index:03}");
        let mut snapshot = fixture
            .prepare_snapshot(
                &run_id,
                WorkflowTaskStatus::Completed,
                WORKFLOW_SOURCE,
                sha256(WORKFLOW_SOURCE),
                i64::try_from(index).unwrap(),
            )
            .await;
        let output_file = snapshot_path(&snapshot).unwrap();
        snapshot.output_file = output_file.clone();
        if run_id == resumable_run_id {
            FileWorkflowJournal::open(
                journal_path(&snapshot.transcript_dir, &snapshot.task_id),
                /*replay_path*/ None,
            )
            .await
            .unwrap();
        }
        if run_id == oldest_run_id {
            snapshot.result_artifact = Some(
                crate::result_artifact::persist_result_artifact(
                    &output_file,
                    Arc::<str>::from(r#"{"answer":"oldest"}"#),
                )
                .await
                .unwrap(),
            );
        }
        let script = validate_workflow_script(WORKFLOW_SOURCE).unwrap();
        write_current_snapshot(
            &output_file,
            &snapshot,
            &PersistedWorkflowExecutionContext::capture(
                &fixture.config,
                fixture.thread_id,
                WorkflowEnvironmentLocation::Local,
                &[local_environment(&fixture.config)],
            )
            .await,
            &PersistedWorkflowComposition::empty(&script),
        )
        .await
        .unwrap();
    }

    let restarted = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
    restarted
        .restore_thread(
            fixture.thread_id,
            fixture.config.clone(),
            AgentRunner::new(Weak::<ThreadManager>::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        restarted
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal_lru
            .len(),
        MAX_RETAINED_TERMINAL_TASKS
    );

    let oldest = restarted
        .wait_for_terminal(fixture.thread_id, oldest_run_id, Duration::ZERO)
        .await
        .unwrap();
    assert!(!oldest.timed_out);
    assert_eq!(oldest.snapshot.status, WorkflowTaskStatus::Completed);
    let chunk = restarted
        .read_result_chunk(fixture.thread_id, &oldest.snapshot, /*offset*/ 0, 128)
        .await
        .unwrap();
    assert_eq!(chunk.text, r#"{"answer":"oldest"}"#);

    let snapshots = restarted.list(fixture.thread_id).await.unwrap();
    assert_eq!(snapshots.len(), history_len);
    assert!(
        snapshots
            .iter()
            .any(|snapshot| snapshot.run_id == oldest_run_id)
    );
    assert_eq!(
        restarted
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal_lru
            .len(),
        MAX_RETAINED_TERMINAL_TASKS
    );

    let script = validate_workflow_script(WORKFLOW_SOURCE).unwrap();
    let composition = FrozenWorkflowComposition::empty(&script);
    let resumed = restarted
        .launch(WorkflowLaunchRequest {
            thread_id: fixture.thread_id,
            turn_id: "turn-history-resume".to_string(),
            config: fixture.config.clone(),
            resolved: ResolvedWorkflow {
                script,
                args: JsonValue::Null,
                resume_from_run_id: Some(resumable_run_id.to_string()),
                origin: crate::discovery::WorkflowOrigin::Inline,
                shadows_existing: false,
                composition,
            },
            agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
            environments: vec![local_environment(&fixture.config)],
            captured_environments: None,
            environment_location: WorkflowEnvironmentLocation::Local,
        })
        .await
        .unwrap();
    assert_eq!(resumed.run_id, resumable_run_id);
    let resumed_outcome = restarted
        .wait_for_terminal(fixture.thread_id, resumable_run_id, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        resumed_outcome.snapshot.status,
        WorkflowTaskStatus::Completed
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
        fixture.service.list(fixture.thread_id).await.unwrap(),
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
async fn tampered_frozen_child_artifact_pauses_restoration() {
    let fixture = RestoreFixture::new().await;
    let source = "export const meta = { name: 'restore-test', description: 'frozen child' }; return workflow({ scriptPath: 'child.js' })";
    let child_source =
        "export const meta = { name: 'child', description: 'approved child' }; return 'ok'";
    let snapshot = fixture
        .persist_current_with_frozen_child(WorkflowTaskStatus::Running, source, child_source)
        .await;
    let composition = load_workflow_metadata(&snapshot).await.unwrap().composition;
    let artifact = workflow_children_dir(&snapshot)
        .unwrap()
        .join(&composition.children[0].artifact_file);
    tokio::fs::write(artifact, child_source.replace("'ok'", "'tampered'"))
        .await
        .unwrap();

    fixture.restore().await;

    let snapshots = fixture.service.list(fixture.thread_id).await.unwrap();
    let [restored] = snapshots.as_slice() else {
        panic!("expected one restored workflow");
    };
    assert_eq!(restored.status, WorkflowTaskStatus::Paused);
    assert!(
        restored
            .error
            .as_deref()
            .is_some_and(|error| error.contains("failed SHA-256 verification"))
    );
}

#[tokio::test]
async fn pauses_restored_remote_active_snapshot() {
    let fixture = RestoreFixture::new().await;
    let mut expected = fixture
        .persist_current(
            WorkflowTaskStatus::Running,
            WORKFLOW_SOURCE,
            WorkflowEnvironmentLocation::Remote,
            &[local_environment(&fixture.config)],
        )
        .await;

    tokio::fs::remove_file(&expected.script_path).await.unwrap();
    fixture.restore().await;

    expected.status = WorkflowTaskStatus::Paused;
    expected.summary = "Workflow restore-test paused".to_string();
    expected.error = Some(
        "remote workflow execution environment is unavailable after restoration; resume explicitly with the Workflow tool to recapture the current environment"
            .to_string(),
    );
    expected.output_file = snapshot_path(&expected).unwrap();
    assert_eq!(
        fixture.service.list(fixture.thread_id).await.unwrap(),
        vec![expected.clone()]
    );
    let persisted: WorkflowTaskSnapshot =
        serde_json::from_slice(&tokio::fs::read(&expected.output_file).await.unwrap()).unwrap();
    assert_eq!(persisted, expected);
}

#[tokio::test]
async fn ignores_snapshot_without_current_metadata() {
    let fixture = RestoreFixture::new().await;
    let snapshot = fixture
        .persist(WorkflowTaskStatus::Running, WORKFLOW_SOURCE)
        .await;
    let path = snapshot_path(&snapshot).unwrap();
    crate::persistence::write_json(&path, &snapshot)
        .await
        .unwrap();

    fixture.restore().await;

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
async fn pauses_current_active_snapshot_with_incompatible_execution_paths() {
    let mut fixture = RestoreFixture::new().await;
    let environment = local_environment(&fixture.config);
    let captured_context = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        std::slice::from_ref(&environment),
    )
    .await;
    let mut expected = fixture
        .persist_current(
            WorkflowTaskStatus::Running,
            WORKFLOW_SOURCE,
            WorkflowEnvironmentLocation::Local,
            std::slice::from_ref(&environment),
        )
        .await;
    fixture.config.cwd = fixture.config.cwd.join("moved-workspace");
    let restored_context = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        std::slice::from_ref(&environment),
    )
    .await;

    fixture.restore().await;

    expected.status = WorkflowTaskStatus::Paused;
    expected.summary = "Workflow restore-test paused".to_string();
    expected.error = Some(format!(
        "captured workflow execution context is incompatible with the restored thread: captured cwd {} with workspace roots {:?}, restored cwd {} with workspace roots {:?}",
        captured_context.cwd,
        captured_context.permission_workspace_roots,
        restored_context.cwd,
        restored_context.permission_workspace_roots,
    ));
    expected.output_file = snapshot_path(&expected).unwrap();
    assert_eq!(
        fixture.service.list(fixture.thread_id).await.unwrap(),
        vec![expected]
    );
}

#[tokio::test]
async fn pauses_current_active_snapshot_when_the_owning_model_changes() {
    let mut fixture = RestoreFixture::new().await;
    fixture.config.model = Some("owning-model-a".to_string());
    let environment = local_environment(&fixture.config);
    let mut expected = fixture
        .persist_current(
            WorkflowTaskStatus::Running,
            WORKFLOW_SOURCE,
            WorkflowEnvironmentLocation::Local,
            std::slice::from_ref(&environment),
        )
        .await;
    fixture.config.model = Some("owning-model-b".to_string());

    fixture.restore().await;

    expected.status = WorkflowTaskStatus::Paused;
    expected.summary = "Workflow restore-test paused".to_string();
    expected.error = Some(
        "captured workflow execution identity changed; resume explicitly with the Workflow tool to use the current workspace and configuration"
            .to_string(),
    );
    expected.output_file = snapshot_path(&expected).unwrap();
    assert_eq!(
        fixture.service.list(fixture.thread_id).await.unwrap(),
        vec![expected]
    );
}

#[tokio::test]
async fn restores_current_local_snapshot_with_captured_uri_selections() {
    let mut fixture = RestoreFixture::new().await;
    let environment = local_environment(&fixture.config);
    let source = "export const meta = { name: 'restore-test', description: 'restore an agent run' }; return agentSettled('restored agent')";
    let mut persisted = fixture
        .persist_current(
            WorkflowTaskStatus::Running,
            source,
            WorkflowEnvironmentLocation::Local,
            std::slice::from_ref(&environment),
        )
        .await;
    persisted.output_file = snapshot_path(&persisted).unwrap();

    let context = load_workflow_metadata(&persisted)
        .await
        .unwrap()
        .execution_context;
    assert_eq!(
        context
            .restore_local_selections(&fixture.config, fixture.thread_id)
            .await
            .unwrap(),
        vec![environment]
    );

    fixture.restore().await;

    let completed = tokio::time::timeout(Duration::from_secs(5), async {
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
    assert_eq!(completed.usage.agent_count, 1);
    assert!(completed.failures[0].contains("thread manager dropped"));
}

#[cfg(unix)]
#[tokio::test]
async fn external_workspace_symlink_conservatively_disables_replay() {
    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    std::fs::write(external.path().join("outside.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(external.path(), root.path().join("external-link")).unwrap();
    let root = AbsolutePathBuf::try_from(root.path().to_path_buf()).unwrap();
    let selection = PersistedTurnEnvironmentSelection {
        environment_id: "local".to_string(),
        cwd: PathUri::from_abs_path(&root),
        workspace_roots: vec![PathUri::from_abs_path(&root)],
        config: PersistedEnvironmentConfigState::FromThread,
    };

    assert_eq!(
        workspace_fingerprint(
            PersistedWorkflowEnvironmentLocation::Local,
            &[selection],
            root.join("sessions").to_path_buf(),
        )
        .await,
        None
    );
}

#[tokio::test]
async fn restored_workflow_preserves_cumulative_usage_telemetry() {
    let mut fixture = RestoreFixture::new().await;
    let source = "export const meta = { name: 'restore-usage', description: 'restore usage telemetry' }; return 'restored'";
    let mut persisted = fixture
        .prepare_snapshot(
            "wf_restore-usage",
            WorkflowTaskStatus::Running,
            source,
            sha256(source),
            100,
        )
        .await;
    persisted.workflow_name = "restore-usage".to_string();
    persisted.usage = WorkflowUsage {
        total_tokens: 37,
        tool_uses: 4,
        duration_ms: 200,
        agent_count: 0,
    };
    persisted.output_file = snapshot_path(&persisted).unwrap();
    let script = validate_workflow_script(source).unwrap();
    let environment = local_environment(&fixture.config);
    write_current_snapshot(
        &persisted.output_file,
        &persisted,
        &PersistedWorkflowExecutionContext::capture(
            &fixture.config,
            fixture.thread_id,
            WorkflowEnvironmentLocation::Local,
            &[environment],
        )
        .await,
        &PersistedWorkflowComposition::empty(&script),
    )
    .await
    .unwrap();

    fixture.restore().await;

    let completed = fixture.wait_for_completion("wf_restore-usage").await;
    let duration_ms = completed.usage.duration_ms;
    assert!(duration_ms >= 200);
    assert_eq!(
        completed.usage,
        WorkflowUsage {
            total_tokens: 37,
            tool_uses: 4,
            duration_ms,
            agent_count: 0,
        }
    );
}

#[tokio::test]
async fn restored_workflow_persists_new_agent_usage_on_top_of_the_saved_baseline() {
    let fixture = RestoreFixture::new().await;
    let source = "export const meta = { name: 'restore-usage-progress', description: 'restore usage progress' }; return null";
    let mut snapshot = fixture
        .prepare_snapshot(
            "wf_restore-usage-progress",
            WorkflowTaskStatus::Running,
            source,
            sha256(source),
            100,
        )
        .await;
    snapshot.usage = WorkflowUsage {
        total_tokens: 37,
        tool_uses: 4,
        duration_ms: 200,
        agent_count: 0,
    };
    snapshot.output_file = snapshot_path(&snapshot).unwrap();
    let environment = local_environment(&fixture.config);
    let execution_context = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        &[environment],
    )
    .await;
    let composition =
        PersistedWorkflowComposition::empty(&validate_workflow_script(source).unwrap());
    let task = Arc::new(WorkflowTask::new(snapshot, execution_context, composition));

    fixture.service.record_progress(
        &task,
        fixture.thread_id,
        0,
        WorkflowEvent::WorkflowAgent(Box::new(codex_protocol::workflow::WorkflowAgentProgress {
            invocation_id: "restored-worker".to_string(),
            index: 0,
            label: "restored worker".to_string(),
            phase_index: None,
            phase_title: None,
            agent_id: Some("agent-restored".to_string()),
            model: Some("test-model".to_string()),
            fallback_model: None,
            isolation: None,
            state: WorkflowAgentState::Done,
            activity: None,
            blocked: false,
            skipped: false,
            awaiting_decision: false,
            cached: false,
            attempt: 0,
            error: None,
            tokens: Some(7),
            tool_calls: Some(2),
            duration_ms: Some(10),
            result_preview: Some("done".to_string()),
            prompt_preview: "restore usage".to_string(),
            queued_at: 101,
            started_at: Some(101),
            last_progress_at: 102,
        })),
    );
    task.persist_snapshot().await.unwrap();

    let output_file = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .output_file
        .clone();
    let persisted: WorkflowTaskSnapshot =
        serde_json::from_slice(&tokio::fs::read(output_file).await.unwrap()).unwrap();
    assert_eq!(
        persisted.usage,
        WorkflowUsage {
            total_tokens: 44,
            tool_uses: 6,
            duration_ms: 200,
            agent_count: 1,
        }
    );
}

#[test]
fn usage_tracking_accumulates_deltas_without_retaining_agent_history() {
    let previous = codex_protocol::workflow::WorkflowAgentProgress {
        invocation_id: "worker".to_string(),
        index: 4_400,
        label: "worker".to_string(),
        phase_index: None,
        phase_title: None,
        agent_id: None,
        model: None,
        fallback_model: None,
        isolation: None,
        state: WorkflowAgentState::Start,
        activity: None,
        blocked: false,
        skipped: false,
        awaiting_decision: false,
        cached: false,
        attempt: 0,
        error: None,
        tokens: Some(10),
        tool_calls: Some(1),
        duration_ms: None,
        result_preview: None,
        prompt_preview: "test".to_string(),
        queued_at: 1,
        started_at: Some(1),
        last_progress_at: 2,
    };
    let mut current = previous.clone();
    current.tokens = Some(15);
    current.tool_calls = Some(3);
    let mut usage = WorkflowUsage {
        total_tokens: 37,
        tool_uses: 4,
        ..Default::default()
    };
    let mut tracker = WorkflowUsageTracker::new(&usage);
    tracker.record(
        0,
        &WorkflowEvent::WorkflowAgent(Box::new(current.clone())),
        Some(&previous),
        &mut usage,
    );
    let expected = WorkflowUsage {
        total_tokens: 42,
        tool_uses: 6,
        ..Default::default()
    };
    assert_eq!(usage, expected);

    current.cached = true;
    current.tokens = Some(1_000);
    current.tool_calls = Some(100);
    tracker.record(
        0,
        &WorkflowEvent::WorkflowAgent(Box::new(current)),
        Some(&previous),
        &mut usage,
    );
    assert_eq!(usage, expected);
}

#[tokio::test]
async fn journal_open_failure_pauses_that_run_and_restores_later_snapshots() {
    let mut fixture = RestoreFixture::new().await;
    let broken = fixture
        .persist_named(
            "wf_broken-journal",
            WorkflowTaskStatus::Running,
            WORKFLOW_SOURCE,
            sha256(WORKFLOW_SOURCE),
            200,
        )
        .await;
    fixture
        .persist_named(
            "wf_healthy-journal",
            WorkflowTaskStatus::Running,
            WORKFLOW_SOURCE,
            sha256(WORKFLOW_SOURCE),
            100,
        )
        .await;
    tokio::fs::create_dir(journal_path(&broken.transcript_dir, &broken.task_id))
        .await
        .unwrap();

    fixture.restore().await;

    let completed = fixture.wait_for_completion("wf_healthy-journal").await;
    assert_eq!(completed.status, WorkflowTaskStatus::Completed);
    let snapshots = fixture.service.list(fixture.thread_id).await.unwrap();
    let broken = snapshots
        .iter()
        .find(|snapshot| snapshot.run_id == "wf_broken-journal")
        .expect("broken journal snapshot");
    assert_eq!(broken.status, WorkflowTaskStatus::Paused);
    assert!(broken.error.as_deref().is_some_and(|error| {
        error.starts_with("failed to open workflow journal during restoration:")
    }));
    let persisted: WorkflowTaskSnapshot =
        serde_json::from_slice(&tokio::fs::read(&broken.output_file).await.unwrap()).unwrap();
    assert_eq!(&persisted, broken);
}

#[tokio::test]
async fn explicit_resume_replays_journal_only_for_matching_execution_identity() {
    let mut fixture = RestoreFixture::new().await;
    let source = "export const meta = { name: 'journal-identity', description: 'test replay identity' }; return agentSettled('journal identity')";
    let first_environment = local_environment(&fixture.config);
    let first = fixture
        .launch(source, None, vec![first_environment.clone()])
        .await;
    fixture.wait_for_completion(&first.run_id).await;

    let mut changed_environment = first_environment;
    let different_workspace = fixture.config.cwd.join("different-workspace");
    tokio::fs::create_dir(&different_workspace).await.unwrap();
    changed_environment.workspace_roots = vec![PathUri::from_abs_path(&different_workspace)];
    fixture
        .launch(
            source,
            Some(first.run_id.clone()),
            vec![changed_environment.clone()],
        )
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let changed_snapshot = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    let changed_agent = changed_snapshot
        .progress
        .iter()
        .find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("changed-identity agent progress");
    assert!(!changed_agent.cached);

    fixture
        .launch(
            source,
            Some(first.run_id.clone()),
            vec![changed_environment],
        )
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let matching_snapshot = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    let matching_agent = matching_snapshot
        .progress
        .iter()
        .find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("matching-identity agent progress");
    assert!(matching_agent.cached);
}

#[tokio::test]
async fn repeated_matching_resumes_replay_results_from_the_original_run() {
    let mut fixture = RestoreFixture::new().await;
    let source = "export const meta = { name: 'journal-lineage', description: 'test replay lineage' }; return agentSettled('journal lineage')";
    let environment = local_environment(&fixture.config);
    let first = fixture
        .launch(source, None, vec![environment.clone()])
        .await;
    fixture.wait_for_completion(&first.run_id).await;

    fixture
        .launch(
            source,
            Some(first.run_id.clone()),
            vec![environment.clone()],
        )
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let first_resume = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    assert!(first_resume.progress.iter().any(|progress| matches!(
        progress,
        WorkflowProgressItem::WorkflowAgent(agent) if agent.cached
    )));

    fixture
        .launch(source, Some(first.run_id.clone()), vec![environment])
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let second_resume = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    assert!(second_resume.progress.iter().any(|progress| matches!(
        progress,
        WorkflowProgressItem::WorkflowAgent(agent) if agent.cached
    )));
}

#[tokio::test]
async fn explicit_resume_replays_only_when_workspace_contents_match() {
    let mut fixture = RestoreFixture::new().await;
    let source = "export const meta = { name: 'workspace-identity', description: 'workspace replay identity' }; return agentSettled('workspace identity')";
    let workspace_file = fixture.config.cwd.join("workspace-input.txt");
    tokio::fs::write(&workspace_file, "revision one")
        .await
        .unwrap();
    let environment = local_environment(&fixture.config);
    let first = fixture
        .launch(source, None, vec![environment.clone()])
        .await;
    fixture.wait_for_completion(&first.run_id).await;

    tokio::fs::write(&workspace_file, "revision two")
        .await
        .unwrap();
    fixture
        .launch(
            source,
            Some(first.run_id.clone()),
            vec![environment.clone()],
        )
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let changed_snapshot = fixture
        .service
        .cached_snapshots(fixture.thread_id)
        .remove(0);
    let changed_agent = changed_snapshot
        .progress
        .iter()
        .find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("changed-workspace agent progress");
    assert!(!changed_agent.cached);

    fixture
        .launch(source, Some(first.run_id.clone()), vec![environment])
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let matching_snapshot = fixture
        .service
        .cached_snapshots(fixture.thread_id)
        .remove(0);
    let matching_agent = matching_snapshot
        .progress
        .iter()
        .find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("matching-workspace agent progress");
    assert!(matching_agent.cached);
}

#[tokio::test]
async fn explicit_resume_replays_only_when_workflow_arguments_match() {
    let mut fixture = RestoreFixture::new().await;
    let source = "export const meta = { name: 'args-identity', description: 'argument replay identity' }; return agentSettled('constant prompt')";
    let environment = local_environment(&fixture.config);
    let first = fixture
        .try_launch_with_args(
            source,
            json!({"revision": 1}),
            None,
            vec![environment.clone()],
        )
        .await
        .unwrap();
    fixture.wait_for_completion(&first.run_id).await;

    fixture
        .try_launch_with_args(
            source,
            json!({"revision": 2}),
            Some(first.run_id.clone()),
            vec![environment.clone()],
        )
        .await
        .unwrap();
    fixture.wait_for_completion(&first.run_id).await;
    let changed_snapshot = fixture
        .service
        .cached_snapshots(fixture.thread_id)
        .remove(0);
    let changed_agent = changed_snapshot
        .progress
        .iter()
        .find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("changed-arguments agent progress");
    assert!(!changed_agent.cached);

    fixture
        .try_launch_with_args(
            source,
            json!({"revision": 2}),
            Some(first.run_id.clone()),
            vec![environment],
        )
        .await
        .unwrap();
    fixture.wait_for_completion(&first.run_id).await;
    let matching_snapshot = fixture
        .service
        .cached_snapshots(fixture.thread_id)
        .remove(0);
    let matching_agent = matching_snapshot
        .progress
        .iter()
        .find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("matching-arguments agent progress");
    assert!(matching_agent.cached);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_resume_launches_commit_exactly_one_replacement() {
    let mut fixture = RestoreFixture::new().await;
    let source = "export const meta = { name: 'concurrent-resume', description: 'reserve one resume' }; return agentSettled('concurrent resume')";
    let environment = local_environment(&fixture.config);
    let first = fixture
        .launch(source, None, vec![environment.clone()])
        .await;
    fixture.wait_for_completion(&first.run_id).await;

    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let launch = |turn_id: &str| {
        let service = fixture.service.clone();
        let config = fixture.config.clone();
        let run_id = first.run_id.clone();
        let environment = environment.clone();
        let barrier = Arc::clone(&barrier);
        let thread_id = fixture.thread_id;
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            let script = validate_workflow_script(source).unwrap();
            let composition = FrozenWorkflowComposition::empty(&script);
            barrier.wait().await;
            service
                .launch(WorkflowLaunchRequest {
                    thread_id,
                    turn_id,
                    config,
                    resolved: ResolvedWorkflow {
                        script,
                        args: JsonValue::Null,
                        resume_from_run_id: Some(run_id),
                        origin: crate::discovery::WorkflowOrigin::Inline,
                        shadows_existing: false,
                        composition,
                    },
                    agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
                    environments: vec![environment],
                    captured_environments: None,
                    environment_location: WorkflowEnvironmentLocation::Local,
                })
                .await
        })
    };
    let first_launch = launch("turn-concurrent-a");
    let second_launch = launch("turn-concurrent-b");
    barrier.wait().await;
    let results = [first_launch.await.unwrap(), second_launch.await.unwrap()];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(WorkflowServiceError::StillRunning)))
            .count(),
        1
    );
    let launch = results.into_iter().find_map(Result::ok).unwrap();
    fixture.wait_for_completion(&launch.run_id).await;
    let snapshots = fixture.service.cached_snapshots(fixture.thread_id);
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].run_id, first.run_id);
    assert_eq!(snapshots[0].thread_id, fixture.thread_id.to_string());
}

#[tokio::test]
async fn explicit_resume_replays_journal_only_for_matching_child_composition() {
    let mut fixture = RestoreFixture::new().await;
    let parent = "export const meta = { name: 'composition-journal', description: 'composition replay identity' }; return workflow({ scriptPath: 'child.js' })";
    let first_child = "export const meta = { name: 'child', description: 'first approved child' }; return agentSettled('composition journal')";
    let changed_child = "export const meta = { name: 'child', description: 'changed approved child' }; return agentSettled('composition journal')";
    let environment = local_environment(&fixture.config);
    let first = fixture
        .launch_with_frozen_path_child(parent, first_child, None, vec![environment.clone()])
        .await;
    fixture.wait_for_completion(&first.run_id).await;

    fixture
        .launch_with_frozen_path_child(
            parent,
            changed_child,
            Some(first.run_id.clone()),
            vec![environment.clone()],
        )
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let changed_snapshot = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    let changed_agent = changed_snapshot
        .progress
        .iter()
        .find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("changed-composition agent progress");
    assert!(!changed_agent.cached);

    fixture
        .launch_with_frozen_path_child(
            parent,
            changed_child,
            Some(first.run_id.clone()),
            vec![environment],
        )
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let matching_snapshot = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    let matching_agent = matching_snapshot
        .progress
        .iter()
        .find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("matching-composition agent progress");
    assert!(matching_agent.cached);
}

#[tokio::test]
async fn identity_change_starts_a_new_journal_generation() {
    let mut fixture = RestoreFixture::new().await;
    let agent_source = "export const meta = { name: 'journal-rotation', description: 'test journal rotation' }; return agentSettled('journal identity')";
    let no_agent_source = "export const meta = { name: 'journal-rotation', description: 'test journal rotation' }; return 'new identity without agents'";
    let environment = local_environment(&fixture.config);
    fixture.config.model = Some("model-a".to_string());
    let first = fixture
        .launch(agent_source, None, vec![environment.clone()])
        .await;
    fixture.wait_for_completion(&first.run_id).await;

    fixture.config.model = Some("model-b".to_string());
    fixture
        .launch(
            no_agent_source,
            Some(first.run_id.clone()),
            vec![environment.clone()],
        )
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    fixture
        .launch(agent_source, Some(first.run_id.clone()), vec![environment])
        .await;
    fixture.wait_for_completion(&first.run_id).await;

    let snapshot = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    let agent = snapshot
        .progress
        .iter()
        .find_map(|progress| match progress {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("agent progress after identity rotation");
    assert!(!agent.cached);
}

#[tokio::test]
async fn failed_resume_commit_preserves_the_old_task_and_replay_journal() {
    let mut fixture = RestoreFixture::new().await;
    let source = "export const meta = { name: 'resume-transaction', description: 'resume commit boundary' }; return agentSettled('resume transaction')";
    let environment = local_environment(&fixture.config);
    fixture.config.model = Some("identity-a".to_string());
    let first = fixture
        .launch(source, None, vec![environment.clone()])
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let old_snapshot = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    let old_snapshot_bytes = tokio::fs::read(&old_snapshot.output_file).await.unwrap();
    let old_journal_path = journal_path(&old_snapshot.transcript_dir, &old_snapshot.task_id);
    let old_journal_bytes = tokio::fs::read(&old_journal_path).await.unwrap();
    let snapshot_backup = old_snapshot.output_file.with_extension("json.backup");
    tokio::fs::rename(&old_snapshot.output_file, &snapshot_backup)
        .await
        .unwrap();
    tokio::fs::create_dir(&old_snapshot.output_file)
        .await
        .unwrap();

    fixture.config.model = Some("identity-b".to_string());
    let error = fixture
        .try_launch(
            source,
            Some(first.run_id.clone()),
            vec![environment.clone()],
        )
        .await
        .unwrap_err();
    assert!(matches!(error, WorkflowServiceError::Persistence(_)));
    tokio::fs::remove_dir(&old_snapshot.output_file)
        .await
        .unwrap();
    tokio::fs::rename(&snapshot_backup, &old_snapshot.output_file)
        .await
        .unwrap();

    assert_eq!(
        tokio::fs::read(&old_snapshot.output_file).await.unwrap(),
        old_snapshot_bytes
    );
    assert_eq!(
        tokio::fs::read(&old_journal_path).await.unwrap(),
        old_journal_bytes
    );
    let old_execution_context = fixture
        .service
        .cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .tasks
        .get(&WorkflowTaskKey::new(fixture.thread_id, &first.run_id))
        .unwrap()
        .execution_context
        .clone();
    assert_eq!(
        fixture.service.list(fixture.thread_id).await.unwrap(),
        vec![old_snapshot.clone()]
    );

    fixture.config.model = Some("identity-a".to_string());
    let current_execution_context = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        std::slice::from_ref(&environment),
    )
    .await;
    assert_eq!(old_execution_context, current_execution_context);
    fixture
        .launch(source, Some(first.run_id.clone()), vec![environment])
        .await;
    fixture.wait_for_completion(&first.run_id).await;
    let resumed = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(resumed.transcript_dir, old_snapshot.transcript_dir);
    let agent = resumed
        .progress
        .iter()
        .find_map(|item| match item {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .expect("resumed agent progress");
    assert!(agent.cached);
}

#[tokio::test]
async fn execution_context_captures_model_provider_and_role_identity() {
    let mut fixture = RestoreFixture::new().await;
    let role_file = fixture.config.cwd.join("reviewer.toml");
    tokio::fs::write(&role_file, "model = 'role-model-a'")
        .await
        .unwrap();
    fixture.config.model = Some("owning-model".to_string());
    fixture.config.model_reasoning_effort = Some(ReasoningEffort::High);
    fixture.config.service_tier = Some("fast".to_string());
    fixture.config.model_provider_id = "provider-a".to_string();
    fixture.config.model_provider.name = "Provider A".to_string();
    fixture.config.agent_default_subagent_model = Some("default-child".to_string());
    fixture.config.agent_default_subagent_reasoning_effort = Some(ReasoningEffort::Low);
    fixture.config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
    fixture.config.approvals_reviewer = ApprovalsReviewer::AutoReview;
    fixture.config.base_instructions = Some("base instructions a".to_string());
    fixture.config.agent_roles.insert(
        "reviewer".to_string(),
        AgentRoleConfig {
            description: Some("Reviews code".to_string()),
            config_file: Some(role_file.to_path_buf()),
            nickname_candidates: None,
        },
    );
    let environment = local_environment(&fixture.config);

    let first = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        std::slice::from_ref(&environment),
    )
    .await;
    tokio::fs::write(&role_file, "model = 'role-model-b'")
        .await
        .unwrap();
    let changed_role = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        std::slice::from_ref(&environment),
    )
    .await;

    assert_eq!(first.model.as_deref(), Some("owning-model"));
    assert_eq!(first.reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(first.service_tier.as_deref(), Some("fast"));
    assert_eq!(first.model_provider_id, "provider-a");
    assert_eq!(first.approval_policy, AskForApproval::Never);
    assert_eq!(first.approvals_reviewer, ApprovalsReviewer::AutoReview);
    assert!(!first.effective_config_fingerprint.is_empty());
    assert_eq!(
        first.default_subagent_model.as_deref(),
        Some("default-child")
    );
    assert_eq!(
        first.default_subagent_reasoning_effort,
        Some(ReasoningEffort::Low)
    );
    assert_ne!(
        first.agent_roles_fingerprint,
        changed_role.agent_roles_fingerprint
    );

    let mut first_executor = first.clone();
    first_executor.execution_environment_fingerprint = Some("executor-a".to_string());
    let mut changed_executor = first_executor.clone();
    changed_executor.execution_environment_fingerprint = Some("executor-b".to_string());
    assert!(!first_executor.replay_identity_matches(&changed_executor));

    fixture.config.model_provider.name = "Provider B".to_string();
    let changed_provider = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        std::slice::from_ref(&environment),
    )
    .await;
    assert_ne!(
        first.model_provider_fingerprint,
        changed_provider.model_provider_fingerprint
    );

    fixture.config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    fixture.config.approvals_reviewer = ApprovalsReviewer::User;
    fixture.config.base_instructions = Some("base instructions b".to_string());
    let changed_effective_config = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        std::slice::from_ref(&environment),
    )
    .await;
    assert_ne!(
        first.approval_policy,
        changed_effective_config.approval_policy
    );
    assert_ne!(
        first.approvals_reviewer,
        changed_effective_config.approvals_reviewer
    );
    assert_ne!(
        first.effective_config_fingerprint,
        changed_effective_config.effective_config_fingerprint
    );

    let mut changed_tools = fixture.config.clone();
    changed_tools.project_doc_max_bytes += 1;
    changed_tools
        .project_doc_fallback_filenames
        .push("PROJECT.md".to_string());
    changed_tools.web_search_config = Some(Default::default());
    changed_tools.tool_registry.error_on_tool_collisions = true;
    changed_tools.code_mode.default_exec_yield_time_ms += 1;
    changed_tools.model_catalog = Some(Default::default());
    assert_ne!(
        effective_config_fingerprint(&fixture.config),
        effective_config_fingerprint(&changed_tools)
    );

    let remote = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Remote,
        std::slice::from_ref(&environment),
    )
    .await;
    assert_eq!(remote.workspace_fingerprint, None);
    assert!(!remote.replay_identity_matches(&remote));

    let mut unavailable_environment = environment;
    unavailable_environment.workspace_roots = vec![PathUri::from_abs_path(
        &fixture.config.cwd.join("missing-workspace"),
    )];
    let unavailable = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        &[unavailable_environment],
    )
    .await;
    assert_eq!(unavailable.workspace_fingerprint, None);
    assert!(!unavailable.replay_identity_matches(&unavailable));
}

#[tokio::test]
async fn workspace_identity_includes_git_ignored_contents() {
    let fixture = RestoreFixture::new().await;
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&*fixture.config.cwd)
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
    tokio::fs::write(fixture.config.cwd.join(".gitignore"), "ignored.txt\n")
        .await
        .unwrap();
    let ignored = fixture.config.cwd.join("ignored.txt");
    tokio::fs::write(&ignored, "revision one").await.unwrap();
    let environment = local_environment(&fixture.config);
    let first = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        std::slice::from_ref(&environment),
    )
    .await;

    tokio::fs::write(&ignored, "revision two").await.unwrap();
    let changed = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        std::slice::from_ref(&environment),
    )
    .await;

    assert_ne!(first.workspace_fingerprint, changed.workspace_fingerprint);
}

#[tokio::test]
async fn current_snapshot_persists_version_and_uri_context_in_main_json() {
    let fixture = RestoreFixture::new().await;
    let environment = local_environment(&fixture.config);
    let mut snapshot = fixture
        .persist_current(
            WorkflowTaskStatus::Running,
            WORKFLOW_SOURCE,
            WorkflowEnvironmentLocation::Local,
            std::slice::from_ref(&environment),
        )
        .await;
    let path = snapshot_path(&snapshot).unwrap();
    snapshot.output_file = path.clone();
    let metadata = load_workflow_metadata(&snapshot).await.unwrap();
    let execution_context = metadata.execution_context;
    let composition = metadata.composition;
    let task = WorkflowTask::new(snapshot.clone(), execution_context, composition);
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .progress_version = 7;
    tokio::fs::write(&path, b"corrupted snapshot")
        .await
        .unwrap();

    task.persist_snapshot().await.unwrap();

    let value: JsonValue = serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();

    assert_eq!(value["progressVersion"], json!(7));
    assert!(value["composition"]["definitionSha256"].is_string());
    assert_eq!(value["composition"]["children"], json!([]));
    assert_eq!(value["executionContext"]["location"], json!("local"));
    assert_eq!(
        value["executionContext"]["selections"],
        serde_json::to_value([PersistedTurnEnvironmentSelection::from(&environment)]).unwrap()
    );
    let mut former_sidecar = snapshot_path(&snapshot).unwrap().to_path_buf();
    former_sidecar.set_extension("environment");
    assert!(!former_sidecar.exists());
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
    let composition = FrozenWorkflowComposition::empty(&edited_script);

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
                composition,
            },
            agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
            environments: Vec::new(),
            captured_environments: None,
            environment_location: WorkflowEnvironmentLocation::Local,
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
    let snapshot = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        read_snapshot_result(&snapshot).await,
        json!("edited result")
    );
    assert_eq!(snapshot.script_sha256, sha256(edited_source));
}

#[tokio::test]
async fn terminal_agent_failure_fails_with_null_and_preserves_diagnostics() {
    let mut fixture = RestoreFixture::new().await;
    let script = validate_workflow_script(
        "export const meta = { name: 'failure-test', description: 'record failure' }; return agent('fail')",
    )
    .unwrap();
    let composition = FrozenWorkflowComposition::empty(&script);
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
                composition,
            },
            agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
            environments: Vec::new(),
            captured_environments: None,
            environment_location: WorkflowEnvironmentLocation::Local,
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

    assert_eq!(completed.status, WorkflowTaskStatus::Failed);
    assert_eq!(completed.usage.agent_count, 1);
    assert_eq!(completed.failures.len(), 1);
    assert!(completed.failures[0].contains("thread manager dropped"));
    let snapshot = fixture
        .service
        .list(fixture.thread_id)
        .await
        .unwrap()
        .into_iter()
        .find(|snapshot| snapshot.run_id == launch.run_id)
        .expect("completed workflow snapshot");
    assert_eq!(read_snapshot_result(&snapshot).await, JsonValue::Null);
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
fn task_cache_keeps_active_tasks_and_recently_used_terminal_tasks() {
    let root = tempfile::tempdir().unwrap();
    let mut cache = WorkflowTaskCache::default();
    for index in 0..MAX_RETAINED_TERMINAL_TASKS {
        let run_id = format!("wf_{index:03}");
        cache.insert(
            run_id.clone(),
            workflow_task(
                root.path(),
                &run_id,
                i64::try_from(index).unwrap(),
                WorkflowTaskStatus::Completed,
            ),
        );
    }
    let key = |run_id: &str| WorkflowTaskKey {
        thread_id: "test-thread".to_string(),
        run_id: run_id.to_string(),
    };
    assert!(cache.get(&key("wf_000")).is_some());
    cache.insert(
        "wf_overflow".to_string(),
        workflow_task(
            root.path(),
            "wf_overflow",
            i64::try_from(MAX_RETAINED_TERMINAL_TASKS).unwrap(),
            WorkflowTaskStatus::Completed,
        ),
    );
    cache.insert(
        "wf_active".to_string(),
        workflow_task(root.path(), "wf_active", 0, WorkflowTaskStatus::Running),
    );

    assert!(cache.tasks.contains_key(&key("wf_000")));
    assert!(!cache.tasks.contains_key(&key("wf_001")));
    assert!(cache.tasks.contains_key(&key("wf_overflow")));
    assert!(cache.tasks.contains_key(&key("wf_active")));
    assert_eq!(cache.terminal_lru.len(), MAX_RETAINED_TERMINAL_TASKS);
}

#[test]
fn thread_codex_home_tracking_has_a_strict_bound() {
    let service = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
    let codex_home = AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let mut first_thread = None;
    for index in 0..=MAX_TRACKED_THREAD_HOMES {
        let thread_id =
            ThreadId::from_string(&format!("00000000-0000-4000-8000-{index:012x}")).unwrap();
        first_thread.get_or_insert(thread_id);
        service.register_thread_codex_home(thread_id, &codex_home);
    }

    assert_eq!(
        service
            .thread_codex_homes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        MAX_TRACKED_THREAD_HOMES
    );
    assert!(service.thread_codex_home(first_thread.unwrap()).is_none());
}

#[tokio::test]
async fn indexed_list_excludes_corrupt_snapshots_and_cursor_survives_status_updates() {
    let fixture = RestoreFixture::new().await;
    let old = fixture
        .persist_named(
            "wf_index-old",
            WorkflowTaskStatus::Completed,
            WORKFLOW_SOURCE,
            sha256(WORKFLOW_SOURCE),
            1,
        )
        .await;
    let middle = fixture
        .persist_named(
            "wf_index-middle",
            WorkflowTaskStatus::Completed,
            WORKFLOW_SOURCE,
            sha256(WORKFLOW_SOURCE),
            2,
        )
        .await;
    let mut newest = fixture
        .persist_named(
            "wf_index-new",
            WorkflowTaskStatus::Completed,
            WORKFLOW_SOURCE,
            sha256(WORKFLOW_SOURCE),
            3,
        )
        .await;
    fixture
        .service
        .register_thread_codex_home(fixture.thread_id, &fixture.config.codex_home);

    let first = fixture
        .service
        .list_page(fixture.thread_id, &[], None, 1)
        .await
        .unwrap();
    assert_eq!(first.snapshots[0].run_id, newest.run_id);
    let cursor = first.next_sequence;
    newest.output_file = snapshot_path(&newest).unwrap();
    newest.status = WorkflowTaskStatus::Failed;
    let metadata = load_workflow_metadata(&newest).await.unwrap();
    write_current_snapshot(
        &newest.output_file,
        &newest,
        &metadata.execution_context,
        &metadata.composition,
    )
    .await
    .unwrap();

    let second = fixture
        .service
        .list_page(fixture.thread_id, &[], cursor, 8)
        .await
        .unwrap();
    assert_eq!(
        second
            .snapshots
            .iter()
            .map(|snapshot| snapshot.run_id.as_str())
            .collect::<Vec<_>>(),
        vec![middle.run_id.as_str(), old.run_id.as_str()]
    );

    tokio::fs::write(snapshot_path(&middle).unwrap(), b"corrupt")
        .await
        .unwrap();
    let listed = fixture
        .service
        .list_page(fixture.thread_id, &[], None, 8)
        .await
        .unwrap();
    assert!(
        listed
            .snapshots
            .iter()
            .all(|snapshot| snapshot.run_id != middle.run_id)
    );
}

#[tokio::test]
async fn indexed_list_repairs_a_snapshot_commit_interrupted_before_index_update() {
    let fixture = RestoreFixture::new().await;
    let mut snapshot = fixture
        .persist_named(
            "wf_index-interrupted",
            WorkflowTaskStatus::Running,
            WORKFLOW_SOURCE,
            sha256(WORKFLOW_SOURCE),
            11,
        )
        .await;
    snapshot.status = WorkflowTaskStatus::Completed;
    snapshot.summary = "Workflow restore-test completed".to_string();
    let metadata = load_workflow_metadata(&snapshot).await.unwrap();
    crate::persistence::write_json(
        &snapshot.output_file,
        &CurrentWorkflowTaskSnapshot {
            snapshot: &snapshot,
            execution_context: &metadata.execution_context,
            composition: &metadata.composition,
        },
    )
    .await
    .unwrap();
    let index_directory = snapshot.output_file.parent().unwrap().join("index");
    crate::persistence::write_json(
        index_directory.join(".dirty.json"),
        &json!({
            "sequence": 0,
            "runId": snapshot.run_id.clone(),
            "firstStartedAt": snapshot.started_at,
        }),
    )
    .await
    .unwrap();
    fixture
        .service
        .register_thread_codex_home(fixture.thread_id, &fixture.config.codex_home);

    let page = fixture
        .service
        .list_page(fixture.thread_id, &[WorkflowTaskStatus::Completed], None, 8)
        .await
        .unwrap();

    assert_eq!(page.snapshots, vec![snapshot]);
    assert_eq!(page.total_matched, 1);
    assert!(!index_directory.join(".dirty.json").exists());
}

#[tokio::test]
async fn restore_retains_the_newest_paused_tasks_beyond_cache_capacity() {
    let fixture = RestoreFixture::new().await;
    let script = validate_workflow_script(WORKFLOW_SOURCE).unwrap();
    let environment = local_environment(&fixture.config);
    let execution_context = PersistedWorkflowExecutionContext::capture(
        &fixture.config,
        fixture.thread_id,
        WorkflowEnvironmentLocation::Local,
        &[environment],
    )
    .await;
    let composition = PersistedWorkflowComposition::empty(&script);
    for index in 0..(MAX_RETAINED_TERMINAL_TASKS + 4) {
        let snapshot = fixture
            .prepare_snapshot(
                &format!("wf_paused-{index:03}"),
                WorkflowTaskStatus::Paused,
                WORKFLOW_SOURCE,
                sha256(WORKFLOW_SOURCE),
                i64::try_from(index).unwrap(),
            )
            .await;
        write_current_snapshot(
            snapshot_path(&snapshot).unwrap(),
            &snapshot,
            &execution_context,
            &composition,
        )
        .await
        .unwrap();
    }

    fixture
        .service
        .restore_thread(
            fixture.thread_id,
            fixture.config.clone(),
            AgentRunner::new(Weak::<ThreadManager>::new()),
        )
        .await
        .unwrap();
    let cache = fixture
        .service
        .cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(cache.tasks.len(), MAX_RETAINED_TERMINAL_TASKS);
    assert!(
        cache
            .tasks
            .contains_key(&WorkflowTaskKey::new(fixture.thread_id, "wf_paused-259"))
    );
    assert!(
        !cache
            .tasks
            .contains_key(&WorkflowTaskKey::new(fixture.thread_id, "wf_paused-000"))
    );
}

#[tokio::test]
async fn repeated_result_reads_are_stable() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    let snapshot_path = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .output_file
        .clone();
    let artifact = crate::result_artifact::persist_result_artifact(
        &snapshot_path,
        Arc::<str>::from(r#"{"version":1}"#),
    )
    .await
    .unwrap();
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .result_artifact = Some(artifact);
    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    let first = service
        .read_result_chunk(thread_id, &snapshot, 0, 512)
        .await
        .unwrap();
    let second = service
        .read_result_chunk(thread_id, &snapshot, 0, 512)
        .await
        .unwrap();

    assert_eq!(first, second);
    assert_eq!(second.text, r#"{"version":1}"#);
}

#[tokio::test]
async fn paged_utf8_result_reads_reuse_the_verified_artifact_cache() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    let snapshot_path = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .output_file
        .clone();
    let serialized =
        Arc::<str>::from(serde_json::to_string(&json!({"value": "你好世界".repeat(40)})).unwrap());
    let artifact =
        crate::result_artifact::persist_result_artifact(&snapshot_path, Arc::clone(&serialized))
            .await
            .unwrap();
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .result_artifact = Some(artifact.clone());
    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    let first = service
        .read_result_chunk(
            thread_id, &snapshot, /*offset*/ 0, /*max_bytes*/ 7,
        )
        .await
        .unwrap();
    let artifact_path = snapshot_path
        .parent()
        .unwrap()
        .join("results")
        .join(artifact.file_name());
    tokio::fs::remove_file(artifact_path).await.unwrap();

    let mut assembled = first.text;
    let mut offset = first.next_offset;
    while offset < first.total_bytes {
        let chunk = service
            .read_result_chunk(thread_id, &snapshot, offset, /*max_bytes*/ 7)
            .await
            .unwrap();
        assert_eq!(chunk.offset, offset);
        assembled.push_str(&chunk.text);
        offset = chunk.next_offset;
    }

    assert_eq!(assembled, serialized.as_ref());
    assert_eq!(offset, u64::try_from(serialized.len()).unwrap());
    assert_eq!(
        task.verified_result
            .lock()
            .await
            .as_ref()
            .map(VerifiedWorkflowResult::artifact),
        Some(&artifact)
    );
}

#[tokio::test]
async fn result_pages_remain_bound_to_first_verified_content_after_same_size_replacement() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    let snapshot_path = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .output_file
        .clone();
    let original = Arc::<str>::from(r#"{"payload":"original"}"#);
    let replacement = r#"{"payload":"replaced"}"#;
    assert_eq!(original.len(), replacement.len());
    let artifact =
        crate::result_artifact::persist_result_artifact(&snapshot_path, Arc::clone(&original))
            .await
            .unwrap();
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .result_artifact = Some(artifact.clone());
    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    let first = service
        .read_result_chunk(thread_id, &snapshot, 0, 8)
        .await
        .unwrap();
    let artifact_path = snapshot_path
        .parent()
        .unwrap()
        .join("results")
        .join(artifact.file_name());
    tokio::fs::write(&artifact_path, replacement).await.unwrap();
    let second = service
        .read_result_chunk(thread_id, &snapshot, first.next_offset, 8)
        .await
        .unwrap();

    assert_eq!(
        format!("{}{}", first.text, second.text),
        original[..usize::try_from(second.next_offset).unwrap()].to_string()
    );
}

#[test]
fn execution_generation_replaces_old_logs_with_authoritative_runtime_window() {
    let root = tempfile::tempdir().unwrap();
    let task = workflow_task(root.path(), "wf_logs", 1, WorkflowTaskStatus::Running);
    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut progress = WorkflowProgressState::from_snapshot(&snapshot);
    progress.record(
        0,
        WorkflowProgressItem::WorkflowLog {
            message: "old generation".to_string(),
        },
    );
    progress.begin_execution(1);
    progress.replace_logs(
        1,
        vec![
            "head".to_string(),
            "[dropped 17 earlier workflow log messages]".to_string(),
            "tail".to_string(),
        ],
    );

    assert_eq!(
        progress
            .latest_window()
            .into_iter()
            .filter_map(|item| match item {
                WorkflowProgressItem::WorkflowLog { message } => Some(message),
                WorkflowProgressItem::WorkflowPhase { .. }
                | WorkflowProgressItem::WorkflowAgent(_) => None,
            })
            .collect::<Vec<_>>(),
        vec![
            "head".to_string(),
            "[dropped 17 earlier workflow log messages]".to_string(),
            "tail".to_string(),
        ]
    );
}

#[test]
fn active_restart_recovers_generation_agent_metadata_and_cold_logs() {
    let root = tempfile::tempdir().unwrap();
    let task = workflow_task(
        root.path(),
        "wf_progress-recovery",
        1,
        WorkflowTaskStatus::Running,
    );
    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut progress = WorkflowProgressState::from_snapshot(&snapshot);
    progress.begin_execution(3);
    progress.record(
        3,
        WorkflowProgressItem::WorkflowAgent(Box::new(test_progress_agent(
            7,
            WorkflowAgentState::Done,
            None,
        ))),
    );
    progress.record(
        3,
        WorkflowProgressItem::WorkflowLog {
            message: "persisted before snapshot".to_string(),
        },
    );
    let stale = PersistedProgressMetadata {
        execution_generation: 2,
        agent_count: 0,
        agent_high_water: 0,
        log_high_water: 0,
        failures: Vec::new(),
    };
    codex_utils_path::write_atomically(
        &progress.metadata_path,
        &serde_json::to_string(&stale).unwrap(),
    )
    .unwrap();
    drop(progress);

    let restored = WorkflowProgressState::from_snapshot(&snapshot);
    assert_eq!(restored.execution_generation(), 3);
    assert_eq!(restored.agent_count(), 1);
    assert_eq!(restored.agent_high_water(), 8);
    assert_eq!(restored.agent(7).unwrap().state, WorkflowAgentState::Done);
    assert!(restored.latest_window().iter().any(|item| {
        matches!(item, WorkflowProgressItem::WorkflowLog { message } if message == "persisted before snapshot")
    }));
}

#[test]
fn active_restart_recovers_generation_from_phase_and_log_records_without_agents() {
    let root = tempfile::tempdir().unwrap();
    let task = workflow_task(
        root.path(),
        "wf_progress-phase-log-recovery",
        1,
        WorkflowTaskStatus::Running,
    );
    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut progress = WorkflowProgressState::from_snapshot(&snapshot);
    progress.begin_execution(4);
    progress.record(
        4,
        WorkflowProgressItem::WorkflowPhase {
            index: 2,
            title: "Recover".to_string(),
            kind: codex_protocol::workflow::WorkflowProgressKind::Active,
        },
    );
    progress.record(
        4,
        WorkflowProgressItem::WorkflowLog {
            message: "phase-only generation".to_string(),
        },
    );
    let stale = PersistedProgressMetadata {
        execution_generation: 3,
        agent_count: 0,
        agent_high_water: 0,
        log_high_water: 0,
        failures: Vec::new(),
    };
    codex_utils_path::write_atomically(
        &progress.metadata_path,
        &serde_json::to_string(&stale).unwrap(),
    )
    .unwrap();
    drop(progress);

    let restored = WorkflowProgressState::from_snapshot(&snapshot);

    assert_eq!(restored.execution_generation(), 4);
    assert!(
        restored
            .latest_window()
            .iter()
            .any(|item| { matches!(item, WorkflowProgressItem::WorkflowPhase { index: 2, .. }) })
    );
    assert!(restored.latest_window().iter().any(|item| {
        matches!(item, WorkflowProgressItem::WorkflowLog { message } if message == "phase-only generation")
    }));
}

#[test]
fn restart_rebuilds_a_bounded_failure_index_from_agent_commits() {
    let root = tempfile::tempdir().unwrap();
    let task = workflow_task(
        root.path(),
        "wf_failure-index",
        1,
        WorkflowTaskStatus::Running,
    );
    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut progress = WorkflowProgressState::from_snapshot(&snapshot);
    for index in 0..(MAX_PROGRESS_FAILURES + 32) {
        progress.record(
            0,
            WorkflowProgressItem::WorkflowAgent(Box::new(test_progress_agent(
                index,
                WorkflowAgentState::Error,
                Some(format!("failure-{index}")),
            ))),
        );
    }
    std::fs::remove_file(&progress.metadata_path).unwrap();
    drop(progress);

    let restored = WorkflowProgressState::from_snapshot(&snapshot);
    assert_eq!(restored.failures().len(), MAX_PROGRESS_FAILURES);
    assert_eq!(restored.agent_count(), MAX_PROGRESS_FAILURES + 32);
}

#[tokio::test]
async fn large_terminal_wait_output_reads_only_a_preview_page() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    let serialized = Arc::<str>::from(
        serde_json::to_string(&json!({ "payload": "x".repeat(200_000) })).unwrap(),
    );
    let snapshot_path = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .output_file
        .clone();
    let artifact =
        crate::result_artifact::persist_result_artifact(&snapshot_path, Arc::clone(&serialized))
            .await
            .unwrap();
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .result_artifact = Some(artifact);

    let status = service
        .wait_for_terminal(thread_id, "wf_wait-test", Duration::ZERO)
        .await
        .unwrap();
    let chunk = service
        .read_result_chunk(
            thread_id,
            &status.snapshot,
            0,
            crate::workflow_result_tool::RESULT_INLINE_MAX_BYTES,
        )
        .await
        .unwrap();
    let output = crate::wait_tool::WaitWorkflowOutput::from_outcome_with_result_chunk(
        status,
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        Some(&chunk),
        /*result_error*/ None,
    )
    .unwrap();

    assert!(!chunk.complete());
    assert!(chunk.text.len() < serialized.len());
    assert_eq!(
        serde_json::to_value(output).unwrap()["resultTruncated"],
        true
    );
}

#[tokio::test]
async fn owning_model_completion_message_delivers_result_without_snapshot_path() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    let serialized =
        Arc::<str>::from(serde_json::to_string(&json!({ "answer": "workflow result" })).unwrap());
    let mut snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let artifact = crate::result_artifact::persist_result_artifact(
        &snapshot.output_file,
        Arc::clone(&serialized),
    )
    .await
    .unwrap();
    snapshot.result_artifact = Some(artifact);
    *task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot.clone();

    let message = service
        .owning_model_completion_message(&snapshot, &workflow_completed_event(&snapshot, thread_id))
        .await;

    assert!(message.contains("workflow result"));
    assert!(message.contains("\"result_available\":true"));
    assert!(message.contains("\"result_truncated\":false"));
    assert!(!message.contains("output_file"));
    assert!(!message.contains(&snapshot.output_file.to_string_lossy().to_string()));
}

#[tokio::test]
async fn owning_model_completion_message_pages_large_results_through_workflow_tool() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    let serialized =
        Arc::<str>::from(serde_json::to_string(&json!({ "answer": "x".repeat(20_000) })).unwrap());
    let mut snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let artifact = crate::result_artifact::persist_result_artifact(
        &snapshot.output_file,
        Arc::clone(&serialized),
    )
    .await
    .unwrap();
    snapshot.result_artifact = Some(artifact);
    *task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot.clone();

    let message = service
        .owning_model_completion_message(&snapshot, &workflow_completed_event(&snapshot, thread_id))
        .await;

    assert!(message.contains("ReadWorkflowResult"));
    assert!(message.contains("\"result_truncated\":true"));
    assert!(message.contains("\"next_offset\":0"));
    assert!(!message.contains("output_file"));
    assert!(!message.contains(&snapshot.output_file.to_string_lossy().to_string()));
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
    task.persist_snapshot().await.unwrap();

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
    Arc::new(WorkflowTask::new(
        WorkflowTaskSnapshot {
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
            result_artifact: None,
            output_file,
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage::default(),
            failures: Vec::new(),
            error: None,
            started_at,
            completed_at: None,
            script_sha256: "test".to_string(),
        },
        test_execution_context(&root),
        PersistedWorkflowComposition::unavailable(),
    ))
}

#[tokio::test]
async fn progress_queue_does_not_poll_slow_persistence_on_the_runtime_task() {
    let root = tempfile::tempdir().unwrap();
    let task = workflow_task(
        root.path(),
        "wf_slow-progress",
        1,
        WorkflowTaskStatus::Running,
    );
    let service = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
    let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
    let blocked_task = Arc::clone(&task);
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let _transition = blocked_task
            .execution_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    locked_rx.recv().unwrap();

    let (sender, sink, worker) =
        start_workflow_progress_worker(service, Arc::clone(&task), thread_id, 0);
    tokio::time::timeout(
        Duration::from_millis(100),
        sink.emit(
            0,
            WorkflowEvent::WorkflowLog {
                message: "queued while persistence is blocked".to_string(),
            },
        ),
    )
    .await
    .expect("progress callback should only wait for bounded queue admission");
    tokio::time::timeout(
        Duration::from_millis(100),
        sink.emit(
            0,
            WorkflowEvent::WorkflowLog {
                message: "queued second".to_string(),
            },
        ),
    )
    .await
    .expect("later progress should preserve queue order without polling persistence");
    assert_eq!(
        task.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .progress_version,
        0
    );

    release_tx.send(()).unwrap();
    blocker.join().unwrap();
    drop(sink);
    drop(sender);
    tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .expect("progress worker should drain after persistence resumes")
        .unwrap();
    assert_eq!(
        task.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .progress_version,
        2
    );
    let messages =
        task.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .progress
            .iter()
            .filter_map(|item| match item {
                WorkflowProgressItem::WorkflowLog { message } => Some(message.clone()),
                WorkflowProgressItem::WorkflowPhase { .. }
                | WorkflowProgressItem::WorkflowAgent(_) => None,
            })
            .collect::<Vec<_>>();
    assert_eq!(
        messages,
        vec![
            "queued while persistence is blocked".to_string(),
            "queued second".to_string(),
        ]
    );
}

#[tokio::test]
async fn wait_for_terminal_returns_an_existing_terminal_workflow_immediately() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);

    let outcome = service
        .wait_for_terminal(thread_id, "wf_wait-test", Duration::ZERO)
        .await
        .unwrap();
    let repeated = service
        .wait_for_terminal(thread_id, "wf_wait-test", Duration::ZERO)
        .await
        .unwrap();

    assert!(!outcome.timed_out);
    assert_eq!(repeated, outcome);
    assert_eq!(outcome.snapshot.status, WorkflowTaskStatus::Completed);
    assert_eq!(
        outcome.snapshot,
        task.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    );
}

#[tokio::test]
async fn wait_for_terminal_blocks_until_terminal_status() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    let waiter = tokio::spawn({
        let service = service.clone();
        async move {
            service
                .wait_for_terminal(thread_id, "wf_wait-test", Duration::from_secs(1))
                .await
        }
    });
    tokio::task::yield_now().await;
    {
        let mut snapshot = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.status = WorkflowTaskStatus::Failed;
        snapshot.error = Some("workflow failed".to_string());
    }
    task.status_tx.send_replace(WorkflowTaskStatus::Failed);

    let outcome = waiter.await.unwrap().unwrap();

    assert!(!outcome.timed_out);
    assert_eq!(outcome.snapshot.status, WorkflowTaskStatus::Failed);
    assert_eq!(outcome.snapshot.error.as_deref(), Some("workflow failed"));
}

#[tokio::test]
async fn wait_for_terminal_reports_timeout_with_current_snapshot() {
    let (service, _task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);

    let outcome = service
        .wait_for_terminal(thread_id, "wf_wait-test", Duration::ZERO)
        .await
        .unwrap();

    assert!(outcome.timed_out);
    assert_eq!(outcome.snapshot.status, WorkflowTaskStatus::Running);
}

#[tokio::test]
async fn wait_for_terminal_never_marks_a_terminal_snapshot_as_timed_out() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .status = WorkflowTaskStatus::Completed;

    let outcome = service
        .wait_for_terminal(thread_id, "wf_wait-test", Duration::ZERO)
        .await
        .unwrap();

    assert!(!outcome.timed_out);
    assert_eq!(outcome.snapshot.status, WorkflowTaskStatus::Completed);
}

#[tokio::test]
async fn terminal_workflows_reject_retry_and_skip_controls() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .progress
        .push(WorkflowProgressItem::WorkflowAgent(Box::new(
            codex_protocol::workflow::WorkflowAgentProgress {
                invocation_id: "worker".to_string(),
                index: 3,
                label: "worker".to_string(),
                phase_index: None,
                phase_title: None,
                agent_id: None,
                model: None,
                fallback_model: None,
                isolation: None,
                state: WorkflowAgentState::Done,
                activity: None,
                blocked: false,
                skipped: false,
                awaiting_decision: false,
                cached: false,
                attempt: 0,
                error: None,
                tokens: None,
                tool_calls: None,
                duration_ms: None,
                result_preview: None,
                prompt_preview: "test".to_string(),
                queued_at: 1,
                started_at: Some(1),
                last_progress_at: 2,
            },
        )));

    assert!(
        !service
            .retry_agent(thread_id, "wf_wait-test", 3)
            .await
            .unwrap()
    );
    assert!(
        !service
            .skip_agent(thread_id, "wf_wait-test", 3)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn stop_reports_submission_time_acceptance() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);

    assert!(service.stop(thread_id, "wf_wait-test").await.unwrap());
    assert!(task.control.is_cancelled());

    task.control.close();
    assert!(!service.stop(thread_id, "wf_wait-test").await.unwrap());
}

#[tokio::test]
async fn settled_agent_retry_reports_submission_time_acceptance() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .progress
        .push(WorkflowProgressItem::WorkflowAgent(Box::new(
            codex_protocol::workflow::WorkflowAgentProgress {
                invocation_id: "worker".to_string(),
                index: 3,
                label: "worker".to_string(),
                phase_index: None,
                phase_title: None,
                agent_id: None,
                model: None,
                fallback_model: None,
                isolation: None,
                state: WorkflowAgentState::Done,
                activity: None,
                blocked: false,
                skipped: false,
                awaiting_decision: false,
                cached: false,
                attempt: 0,
                error: None,
                tokens: None,
                tool_calls: None,
                duration_ms: None,
                result_preview: None,
                prompt_preview: "test".to_string(),
                queued_at: 1,
                started_at: Some(1),
                last_progress_at: 2,
            },
        )));

    let persisted_agents = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .progress
        .iter()
        .filter_map(|item| match item {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.as_ref().clone()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .collect::<Vec<_>>();
    for agent in persisted_agents {
        task.progress_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(
                /*execution_generation*/ 0,
                WorkflowProgressItem::WorkflowAgent(Box::new(agent)),
            );
    }

    assert!(
        service
            .retry_agent(thread_id, "wf_wait-test", 3)
            .await
            .unwrap()
    );
    task.control.close();
    assert!(
        !service
            .retry_agent(thread_id, "wf_wait-test", 3)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn settled_agent_rerun_truncates_progress_without_reducing_cumulative_usage() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    let first = codex_protocol::workflow::WorkflowAgentProgress {
        invocation_id: "branch-point".to_string(),
        index: 2,
        label: "branch point".to_string(),
        phase_index: None,
        phase_title: None,
        agent_id: None,
        model: None,
        fallback_model: None,
        isolation: None,
        state: WorkflowAgentState::Done,
        activity: None,
        blocked: false,
        skipped: false,
        awaiting_decision: false,
        cached: false,
        attempt: 0,
        error: None,
        tokens: Some(10),
        tool_calls: Some(1),
        duration_ms: None,
        result_preview: None,
        prompt_preview: "test".to_string(),
        queued_at: 1,
        started_at: Some(1),
        last_progress_at: 2,
    };
    let downstream = codex_protocol::workflow::WorkflowAgentProgress {
        index: 5,
        label: "old downstream branch".to_string(),
        tokens: Some(20),
        tool_calls: Some(2),
        ..first.clone()
    };
    let progress_snapshot = {
        let mut snapshot = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.progress = vec![
            WorkflowProgressItem::WorkflowAgent(Box::new(first.clone())),
            WorkflowProgressItem::WorkflowAgent(Box::new(downstream.clone())),
        ];
        snapshot.usage = WorkflowUsage {
            total_tokens: 30,
            tool_uses: 3,
            duration_ms: 0,
            agent_count: 2,
        };
        snapshot.clone()
    };
    let mut progress_state = WorkflowProgressState::from_snapshot(&progress_snapshot);
    progress_state.record(
        /*execution_generation*/ 0,
        WorkflowProgressItem::WorkflowAgent(Box::new(first)),
    );
    progress_state.record(
        /*execution_generation*/ 0,
        WorkflowProgressItem::WorkflowAgent(Box::new(downstream)),
    );
    *task
        .progress_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = progress_state;

    assert!(
        service
            .retry_agent(thread_id, "wf_wait-test", 2)
            .await
            .unwrap()
    );

    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        snapshot
            .progress
            .iter()
            .filter_map(|item| match item {
                WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.index),
                WorkflowProgressItem::WorkflowPhase { .. }
                | WorkflowProgressItem::WorkflowLog { .. } => None,
            })
            .collect::<Vec<_>>(),
        Vec::<usize>::new()
    );
    assert_eq!(snapshot.progress_version, 1);
    assert_eq!(
        snapshot.usage,
        WorkflowUsage {
            total_tokens: 30,
            tool_uses: 3,
            duration_ms: 0,
            agent_count: 0,
        }
    );
}

#[tokio::test]
async fn latest_progress_and_control_retain_agents_beyond_the_old_snapshot_threshold() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    let latest_window = {
        let mut progress_state = task
            .progress_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for index in 0..5_000 {
            let agent = codex_protocol::workflow::WorkflowAgentProgress {
                invocation_id: format!("invocation-{index}"),
                index,
                label: format!("agent-{index}"),
                phase_index: None,
                phase_title: None,
                agent_id: None,
                model: None,
                fallback_model: None,
                isolation: None,
                state: WorkflowAgentState::Done,
                activity: None,
                blocked: false,
                skipped: false,
                awaiting_decision: false,
                cached: false,
                attempt: 0,
                error: None,
                tokens: None,
                tool_calls: None,
                duration_ms: None,
                result_preview: None,
                prompt_preview: "test".to_string(),
                queued_at: 1,
                started_at: Some(1),
                last_progress_at: 2,
            };
            progress_state.record(
                /*execution_generation*/ 0,
                WorkflowProgressItem::WorkflowAgent(Box::new(agent.clone())),
            );
        }
        let mut revisited = progress_state.agent(0).unwrap();
        revisited.attempt = 1;
        revisited.last_progress_at = 3;
        progress_state.record(
            /*execution_generation*/ 0,
            WorkflowProgressItem::WorkflowAgent(Box::new(revisited)),
        );
        progress_state.latest_window()
    };
    let persisted_snapshot = {
        let mut snapshot = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.progress = latest_window;
        snapshot.usage.agent_count = 5_000;
        snapshot.clone()
    };
    *task
        .progress_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        WorkflowProgressState::from_snapshot(&persisted_snapshot);

    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(snapshot.progress.len() <= 512);
    assert!(snapshot.progress.iter().any(|item| {
        matches!(item, WorkflowProgressItem::WorkflowAgent(agent) if agent.index == 4_999)
    }));
    assert!(snapshot.progress.iter().any(|item| {
        matches!(item, WorkflowProgressItem::WorkflowAgent(agent) if agent.index == 0)
    }));
    assert!(!snapshot.progress.iter().any(|item| {
        matches!(item, WorkflowProgressItem::WorkflowAgent(agent) if agent.index == 4_400)
    }));
    let late_page = service
        .progress_page(thread_id, "wf_wait-test", 4_900, 100)
        .await
        .unwrap();
    assert_eq!(late_page.agents.len(), 100);
    assert_eq!(late_page.total_agents, 5_000);
    assert_eq!(late_page.next_index, None);
    assert!(late_page.agents.iter().any(|agent| agent.index == 4_999));
    assert_eq!(
        service
            .agent_progress(thread_id, "wf_wait-test", 4_400)
            .await
            .unwrap()
            .map(|agent| agent.index),
        Some(4_400)
    );
    assert!(
        service
            .retry_agent(thread_id, "wf_wait-test", 4_400)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn rerun_ignores_late_progress_from_the_previous_execution_generation() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    let agent = codex_protocol::workflow::WorkflowAgentProgress {
        invocation_id: "branch-point".to_string(),
        index: 2,
        label: "branch point".to_string(),
        phase_index: None,
        phase_title: None,
        agent_id: None,
        model: None,
        fallback_model: None,
        isolation: None,
        state: WorkflowAgentState::Done,
        activity: None,
        blocked: false,
        skipped: false,
        awaiting_decision: false,
        cached: false,
        attempt: 0,
        error: None,
        tokens: None,
        tool_calls: None,
        duration_ms: None,
        result_preview: None,
        prompt_preview: "test".to_string(),
        queued_at: 1,
        started_at: Some(1),
        last_progress_at: 2,
    };
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .progress
        .push(WorkflowProgressItem::WorkflowAgent(Box::new(agent.clone())));
    task.progress_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record(
            /*execution_generation*/ 0,
            WorkflowProgressItem::WorkflowAgent(Box::new(agent.clone())),
        );
    assert!(
        service
            .retry_agent(thread_id, "wf_wait-test", 2)
            .await
            .unwrap()
    );
    let mut late = agent.clone();
    late.invocation_id = "downstream".to_string();
    late.index = 5;
    late.label = "late old execution".to_string();
    late.tokens = Some(40);
    late.tool_calls = Some(4);
    service.record_progress(
        &task,
        thread_id,
        0,
        WorkflowEvent::WorkflowAgent(Box::new(late.clone())),
    );
    service.record_progress(
        &task,
        thread_id,
        0,
        WorkflowEvent::WorkflowAgent(Box::new(late)),
    );
    let mut current = agent;
    current.invocation_id = "downstream".to_string();
    current.index = 5;
    current.label = "current execution".to_string();
    current.tokens = Some(10);
    current.tool_calls = Some(1);
    service.record_progress(
        &task,
        thread_id,
        1,
        WorkflowEvent::WorkflowAgent(Box::new(current)),
    );

    let labels = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .progress
        .iter()
        .filter_map(|item| match item {
            WorkflowProgressItem::WorkflowAgent(agent) => Some(agent.label.clone()),
            WorkflowProgressItem::WorkflowPhase { .. }
            | WorkflowProgressItem::WorkflowLog { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(labels, vec!["current execution"]);
    assert_eq!(
        task.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .usage,
        WorkflowUsage {
            total_tokens: 50,
            tool_uses: 5,
            duration_ms: 0,
            agent_count: 1,
        }
    );
}

#[tokio::test]
async fn old_generation_usage_survives_failed_and_cancelled_reruns() {
    for (suffix, error, expected_status) in [
        (
            "failed",
            WorkflowExecutionError::Runtime("new rerun failed".to_string()),
            WorkflowTaskStatus::Failed,
        ),
        (
            "cancelled",
            WorkflowExecutionError::Cancelled,
            WorkflowTaskStatus::Killed,
        ),
    ] {
        let (base_service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
        drop(base_service);
        let service = WorkflowService::new(
            Arc::new(AcknowledgeEventSink {
                attempts: AtomicUsize::new(0),
            }),
            Weak::new(),
        );
        let run_id = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .run_id
            .clone();
        service.cache_task(run_id, Arc::clone(&task));
        let output_file = {
            let mut snapshot = task
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            snapshot.task_id = format!("wusage-{suffix}");
            snapshot.usage.duration_ms = 200;
            snapshot.output_file.clone()
        };
        tokio::fs::create_dir_all(output_file.parent().unwrap())
            .await
            .unwrap();
        task.execution_generation.store(1, Ordering::Release);
        task.progress_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .begin_execution(1);
        let mut late = test_progress_agent(8, WorkflowAgentState::Done, None);
        late.invocation_id = "old-generation-final".to_string();
        late.tokens = Some(40);
        late.tool_calls = Some(4);
        service.record_progress(
            &task,
            thread_id,
            0,
            WorkflowEvent::WorkflowAgent(Box::new(late.clone())),
        );
        service.record_progress(
            &task,
            thread_id,
            0,
            WorkflowEvent::WorkflowAgent(Box::new(late)),
        );

        service
            .finish_task(Arc::clone(&task), thread_id, Err(error))
            .await;

        let snapshot = task
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(snapshot.status, expected_status);
        assert_eq!(
            snapshot.usage,
            WorkflowUsage {
                total_tokens: 40,
                tool_uses: 4,
                duration_ms: 200,
                agent_count: 0,
            }
        );
    }
}

#[tokio::test]
async fn terminal_snapshot_failure_keeps_running_state_retryable() {
    let root = tempfile::tempdir().unwrap();
    let task = workflow_task(
        root.path(),
        "wf_terminal-retry",
        1,
        WorkflowTaskStatus::Running,
    );
    let mut terminal = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    terminal.status = WorkflowTaskStatus::Completed;
    terminal.completed_at = Some(2);
    let blocked_parent = terminal.output_file.parent().unwrap();
    tokio::fs::write(&blocked_parent, b"not a directory")
        .await
        .unwrap();

    persist_terminal_task(&task, &terminal)
        .await
        .expect_err("blocked snapshot parent must fail persistence");

    let status_rx = task.status_tx.subscribe();
    assert_eq!(*status_rx.borrow(), WorkflowTaskStatus::Running);
    assert_eq!(
        task.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status,
        WorkflowTaskStatus::Running
    );
    {
        let state = task
            .persist_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.dirty);
        assert!(!state.terminal);
    }

    tokio::fs::remove_file(&blocked_parent).await.unwrap();
    tokio::fs::create_dir_all(&blocked_parent).await.unwrap();
    persist_terminal_task(&task, &terminal).await.unwrap();

    assert_eq!(
        task.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status,
        WorkflowTaskStatus::Completed
    );
    assert!(
        task.persist_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal
    );
}

#[tokio::test]
async fn closed_control_rejects_stop_skip_and_settled_retry() {
    let (service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .progress
        .push(WorkflowProgressItem::WorkflowAgent(Box::new(
            codex_protocol::workflow::WorkflowAgentProgress {
                invocation_id: "worker".to_string(),
                index: 3,
                label: "worker".to_string(),
                phase_index: None,
                phase_title: None,
                agent_id: None,
                model: None,
                fallback_model: None,
                isolation: None,
                state: WorkflowAgentState::Done,
                activity: None,
                blocked: false,
                skipped: false,
                awaiting_decision: false,
                cached: false,
                attempt: 0,
                error: None,
                tokens: None,
                tool_calls: None,
                duration_ms: None,
                result_preview: None,
                prompt_preview: "test".to_string(),
                queued_at: 1,
                started_at: Some(1),
                last_progress_at: 2,
            },
        )));
    task.control.close();

    assert!(!service.stop(thread_id, "wf_wait-test").await.unwrap());
    assert!(
        !service
            .skip_agent(thread_id, "wf_wait-test", 3)
            .await
            .unwrap()
    );
    assert!(
        !service
            .retry_agent(thread_id, "wf_wait-test", 3)
            .await
            .unwrap()
    );
}

#[test]
fn stop_racing_with_control_close_has_one_linearized_outcome() {
    for _ in 0..128 {
        let (_service, task, _thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
        let barrier = Arc::new(Barrier::new(3));
        let stop_barrier = Arc::clone(&barrier);
        let stop_control = task.control.clone();
        let stop = std::thread::spawn(move || {
            stop_barrier.wait();
            stop_control.try_stop()
        });
        let close_barrier = Arc::clone(&barrier);
        let control = task.control.clone();
        let close = std::thread::spawn(move || {
            close_barrier.wait();
            control.close();
        });

        barrier.wait();
        let accepted = stop.join().unwrap();
        close.join().unwrap();

        assert_eq!(accepted, task.control.is_cancelled());
        assert!(!task.control.try_stop());
    }
}

#[tokio::test]
async fn wait_for_terminal_rejects_a_workflow_owned_by_another_thread() {
    let (service, _task, _thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    let other_thread_id = ThreadId::from_string("22222222-2222-4222-8222-222222222222").unwrap();

    let error = service
        .wait_for_terminal(other_thread_id, "wf_wait-test", Duration::ZERO)
        .await
        .unwrap_err();

    assert!(matches!(error, WorkflowServiceError::NotFound));
}

#[tokio::test]
async fn terminal_task_keeps_owner_resident_until_completion_delivery_finishes() {
    let codex_home = tempfile::tempdir().unwrap();
    let root = AbsolutePathBuf::try_from(codex_home.path().to_path_buf()).unwrap();
    let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
    let sink = Arc::new(BlockingCompletionEventSink::default());
    let service = WorkflowService::new(sink.clone(), Weak::new());
    let task = Arc::new(WorkflowTask::new(
        WorkflowTaskSnapshot {
            thread_id: thread_id.to_string(),
            turn_id: "turn-residency".to_string(),
            task_id: "wresidency".to_string(),
            run_id: "wf_residency".to_string(),
            workflow_name: "residency-test".to_string(),
            title: None,
            status: WorkflowTaskStatus::Running,
            summary: "running".to_string(),
            transcript_dir: root.join("transcript"),
            script_path: root.join("workflow.js"),
            args: JsonValue::Null,
            result_artifact: None,
            output_file: root.join("workflow.json"),
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage::default(),
            failures: Vec::new(),
            error: None,
            started_at: 1,
            completed_at: None,
            script_sha256: "sha256".to_string(),
        },
        test_execution_context(&root),
        PersistedWorkflowComposition::unavailable(),
    ));
    service.cache_task("wf_residency".to_string(), Arc::clone(&task));

    let finishing = {
        let service = service.clone();
        let task = Arc::clone(&task);
        tokio::spawn(async move {
            service
                .finish_task(task, thread_id, Err(WorkflowExecutionError::Cancelled))
                .await;
        })
    };
    sink.delivery_started.notified().await;

    assert_eq!(
        task.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status,
        WorkflowTaskStatus::Killed
    );
    assert!(service.keeps_thread_resident(thread_id));
    assert!(matches!(
        service.validate_resume(thread_id, "wf_residency").await,
        Err(WorkflowServiceError::StillRunning)
    ));

    sink.release_delivery.notify_one();
    finishing.await.unwrap();

    assert!(!service.keeps_thread_resident(thread_id));
    assert!(
        service
            .validate_resume(thread_id, "wf_residency")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn terminal_task_releases_residency_with_durable_retryable_delivery() {
    let root = tempfile::tempdir().unwrap();
    let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
    let task = workflow_task(
        root.path(),
        "wf_retryable-residency",
        1,
        WorkflowTaskStatus::Running,
    );
    task.snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .thread_id = thread_id.to_string();
    let sink = Arc::new(AlwaysRetryEventSink {
        attempts: AtomicUsize::new(0),
    });
    let service = WorkflowService::new(sink.clone(), Weak::new());
    service.cache_task("wf_retryable-residency".to_string(), Arc::clone(&task));

    service
        .finish_task(
            Arc::clone(&task),
            thread_id,
            Err(WorkflowExecutionError::Cancelled),
        )
        .await;

    let snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(!service.keeps_thread_resident(thread_id));
    assert_eq!(
        sink.attempts.load(Ordering::Acquire),
        LIFECYCLE_DELIVERY_ATTEMPTS
    );
    assert!(
        !load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed)
            .unwrap()
            .transport_acknowledged
    );
}

#[tokio::test]
async fn retryable_completion_delivery_retries_online_and_acknowledges_durably() {
    let (base_service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    drop(base_service);
    let (_delivery_dir, snapshot) = delivery_snapshot(&task);
    let sink = Arc::new(RetryThenAcknowledgeEventSink {
        attempts: AtomicUsize::new(0),
    });
    let service = WorkflowService::new(sink.clone(), Weak::new());
    let event = workflow_completed_event(&snapshot, thread_id);

    service.deliver_completion(&snapshot, event).await;

    let acknowledged =
        load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed).unwrap();
    assert!(acknowledged.transport_acknowledged);
    assert!(!acknowledged.owning_model_acknowledged);
    assert_eq!(sink.attempts.load(Ordering::Acquire), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retryable_transport_does_not_delay_the_owning_model_result() {
    let server = responses::start_mock_server().await;
    let next_request = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("workflow-owner-next"),
            responses::ev_completed("workflow-owner-next"),
        ]),
    )
    .await;
    let test = test_codex().build(&server).await.unwrap();
    let thread_id: ThreadId = test.session_configured.session_id.into();
    let root = tempfile::tempdir().unwrap();
    let task = workflow_task(
        root.path(),
        "wf_retryable-owner",
        1,
        WorkflowTaskStatus::Completed,
    );
    let mut snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    snapshot.thread_id = thread_id.to_string();
    snapshot.completed_at = Some(2);
    let serialized = Arc::<str>::from(
        serde_json::to_string(&json!({ "answer": "available to the owner" })).unwrap(),
    );
    snapshot.result_artifact = Some(
        crate::result_artifact::persist_result_artifact(
            &snapshot.output_file,
            Arc::clone(&serialized),
        )
        .await
        .unwrap(),
    );
    *task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot.clone();
    let sink = Arc::new(AlwaysRetryEventSink {
        attempts: AtomicUsize::new(0),
    });
    let service = WorkflowService::new(sink.clone(), Arc::downgrade(&test.thread_manager));
    service.cache_task(snapshot.run_id.clone(), task);

    assert!(
        service
            .deliver_completion(&snapshot, workflow_completed_event(&snapshot, thread_id))
            .await
    );
    let delivery =
        load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed).unwrap();
    assert!(delivery.owning_model_acknowledged);
    assert!(!delivery.transport_acknowledged);

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "continue".to_string(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();
    core_test_support::wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    let user_messages = next_request.single_request().message_input_texts("user");
    assert!(user_messages.iter().any(|message| {
        message.contains("<workflow_notification>") && message.contains("available to the owner")
    }));
}

#[tokio::test]
async fn retryable_completion_delivery_remains_pending_and_replays_after_restart() {
    let (base_service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    drop(base_service);
    let (_delivery_dir, snapshot) = delivery_snapshot(&task);
    let retrying_sink = Arc::new(AlwaysRetryEventSink {
        attempts: AtomicUsize::new(0),
    });
    let first_service = WorkflowService::new(retrying_sink.clone(), Weak::new());
    let event = workflow_completed_event(&snapshot, thread_id);

    first_service
        .deliver_completion(&snapshot, event.clone())
        .await;

    let pending = load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed).unwrap();
    assert_eq!(
        pending,
        LifecycleDeliveryState {
            idempotency_key: event_idempotency_key(&Event {
                id: event.turn_id.clone(),
                msg: EventMsg::WorkflowCompleted(event.clone()),
            }),
            transport_acknowledged: false,
            owning_model_acknowledged: false,
        }
    );
    assert_eq!(
        retrying_sink.attempts.load(Ordering::Acquire),
        LIFECYCLE_DELIVERY_ATTEMPTS
    );

    let acknowledging_sink = Arc::new(AcknowledgeEventSink {
        attempts: AtomicUsize::new(0),
    });
    let restarted_service = WorkflowService::new(acknowledging_sink.clone(), Weak::new());
    restarted_service.deliver_completion(&snapshot, event).await;

    let acknowledged =
        load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed).unwrap();
    assert!(acknowledged.transport_acknowledged);
    assert!(!acknowledged.owning_model_acknowledged);
    assert_eq!(acknowledging_sink.attempts.load(Ordering::Acquire), 1);

    let second_restart = WorkflowService::new(acknowledging_sink.clone(), Weak::new());
    second_restart
        .deliver_completion(&snapshot, workflow_completed_event(&snapshot, thread_id))
        .await;
    assert_eq!(acknowledging_sink.attempts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn pending_completion_delivers_automatically_after_a_late_reconnect() {
    let (base_service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Completed);
    drop(base_service);
    let (_delivery_dir, snapshot) = delivery_snapshot(&task);
    let sink = Arc::new(ReconnectingEventSink {
        attempts: AtomicUsize::new(0),
        connected: AtomicBool::new(false),
        waiting_for_availability: Notify::new(),
        available: Notify::new(),
        acknowledged: Notify::new(),
    });
    let service = WorkflowService::new(sink.clone(), Weak::new());
    *task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot.clone();
    service.cache_task(snapshot.run_id.clone(), Arc::clone(&task));

    service
        .deliver_completion(&snapshot, workflow_completed_event(&snapshot, thread_id))
        .await;
    assert_eq!(
        sink.attempts.load(Ordering::Acquire),
        LIFECYCLE_DELIVERY_ATTEMPTS
    );
    assert!(
        !load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed)
            .unwrap()
            .transport_acknowledged
    );

    sink.waiting_for_availability.notified().await;
    sink.connected.store(true, Ordering::Release);
    sink.available.notify_one();
    tokio::time::timeout(Duration::from_secs(1), sink.acknowledged.notified())
        .await
        .expect("reconnected delivery should be acknowledged automatically");
    let delivery_lock = lock_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed)
        .await
        .unwrap();
    drop(delivery_lock);

    assert!(
        load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Completed)
            .unwrap()
            .transport_acknowledged
    );
    assert_eq!(
        sink.attempts.load(Ordering::Acquire),
        LIFECYCLE_DELIVERY_ATTEMPTS + 1
    );
}

#[tokio::test]
async fn uncached_pending_completion_is_discovered_from_its_durable_marker() {
    let fixture = RestoreFixture::new().await;
    let mut snapshot = fixture
        .persist(WorkflowTaskStatus::Completed, WORKFLOW_SOURCE)
        .await;
    snapshot.output_file = snapshot_path(&snapshot).unwrap();
    let event = workflow_completed_event(&snapshot, fixture.thread_id);
    persist_lifecycle_delivery(
        &snapshot,
        WorkflowLifecycleDelivery::Completed,
        &LifecycleDeliveryState {
            idempotency_key: event_idempotency_key(&Event {
                id: event.turn_id.clone(),
                msg: EventMsg::WorkflowCompleted(event),
            }),
            transport_acknowledged: false,
            owning_model_acknowledged: false,
        },
    )
    .await
    .unwrap();
    let service = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
    service.register_thread_codex_home(fixture.thread_id, &fixture.config.codex_home);

    let pending = service.pending_delivery_snapshots().await;

    assert_eq!(
        pending
            .iter()
            .map(|snapshot| snapshot.run_id.as_str())
            .collect::<Vec<_>>(),
        vec![snapshot.run_id.as_str()]
    );
    persist_lifecycle_delivery(
        &snapshot,
        WorkflowLifecycleDelivery::Completed,
        &LifecycleDeliveryState {
            idempotency_key: "completed".to_string(),
            transport_acknowledged: true,
            owning_model_acknowledged: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        load_pending_lifecycle_deliveries(
            workflow_session_dir(&fixture.config.codex_home, fixture.thread_id).join("workflows")
        )
        .await
        .unwrap(),
        Vec::new()
    );

    let stale_marker =
        pending_lifecycle_delivery_path(&snapshot, WorkflowLifecycleDelivery::Completed);
    crate::persistence::write_json(
        &stale_marker,
        &PendingLifecycleDelivery {
            run_id: snapshot.run_id.clone(),
            lifecycle: WorkflowLifecycleDelivery::Completed,
        },
    )
    .await
    .unwrap();
    assert!(service.pending_delivery_snapshots().await.is_empty());
    assert!(!stale_marker.exists());
}

#[tokio::test]
async fn started_delivery_retries_online_and_persists_acknowledgment() {
    let (base_service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    drop(base_service);
    let (_delivery_dir, snapshot) = delivery_snapshot(&task);
    let sink = Arc::new(RetryThenAcknowledgeEventSink {
        attempts: AtomicUsize::new(0),
    });
    let service = WorkflowService::new(sink.clone(), Weak::new());

    service.emit_started(&snapshot, thread_id).await;

    let acknowledged =
        load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Started).unwrap();
    assert!(acknowledged.transport_acknowledged);
    assert!(!acknowledged.owning_model_acknowledged);
    assert_eq!(sink.attempts.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn independent_services_serialize_started_delivery_by_persisted_identity() {
    let (base_service, task, thread_id) = wait_service_with_task(WorkflowTaskStatus::Running);
    drop(base_service);
    let (_delivery_dir, snapshot) = delivery_snapshot(&task);
    let sink = Arc::new(AcknowledgeEventSink {
        attempts: AtomicUsize::new(0),
    });
    let first = WorkflowService::new(sink.clone(), Weak::new());
    let second = WorkflowService::new(sink.clone(), Weak::new());

    let (first_delivered, second_delivered) = tokio::join!(
        first.emit_started(&snapshot, thread_id),
        second.emit_started(&snapshot, thread_id),
    );

    assert!(first_delivered);
    assert!(second_delivered);
    assert_eq!(sink.attempts.load(Ordering::Acquire), 1);
    assert!(
        load_lifecycle_delivery(&snapshot, WorkflowLifecycleDelivery::Started)
            .unwrap()
            .transport_acknowledged
    );
}

#[tokio::test]
async fn independent_services_share_persisted_resume_reservation() {
    let fixture = RestoreFixture::new().await;
    let snapshot = fixture
        .persist(WorkflowTaskStatus::Completed, WORKFLOW_SOURCE)
        .await;
    let first = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
    let second = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
    first.register_thread_codex_home(fixture.thread_id, &fixture.config.codex_home);
    second.register_thread_codex_home(fixture.thread_id, &fixture.config.codex_home);

    let reservation = first
        .reserve_resume(fixture.thread_id, &snapshot.run_id)
        .await
        .unwrap();
    assert!(matches!(
        second
            .reserve_resume(fixture.thread_id, &snapshot.run_id)
            .await,
        Err(WorkflowServiceError::StillRunning)
    ));
    drop(reservation);
    assert!(
        second
            .reserve_resume(fixture.thread_id, &snapshot.run_id)
            .await
            .is_ok()
    );
}

#[test]
fn cache_separates_identical_run_ids_owned_by_different_threads() {
    let root = tempfile::tempdir().unwrap();
    let first = workflow_task(
        root.path(),
        "wf_duplicate",
        1,
        WorkflowTaskStatus::Completed,
    );
    let second = workflow_task(root.path(), "wf_duplicate", 2, WorkflowTaskStatus::Running);
    second
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .thread_id = "other-thread".to_string();
    let mut cache = WorkflowTaskCache::default();
    cache.insert("wf_duplicate".to_string(), first);
    cache.insert("wf_duplicate".to_string(), second);

    assert!(
        cache
            .get(&WorkflowTaskKey {
                thread_id: "test-thread".to_string(),
                run_id: "wf_duplicate".to_string(),
            })
            .is_some()
    );
    assert!(
        cache
            .get(&WorkflowTaskKey {
                thread_id: "other-thread".to_string(),
                run_id: "wf_duplicate".to_string(),
            })
            .is_some()
    );
}

#[test]
fn duplicate_run_ids_preserve_thread_scoped_query_and_residency() {
    let root = tempfile::tempdir().unwrap();
    let first_thread = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
    let second_thread = ThreadId::from_string("22222222-2222-4222-8222-222222222222").unwrap();
    let first = workflow_task(
        root.path(),
        "wf_duplicate-residency",
        1,
        WorkflowTaskStatus::Completed,
    );
    first
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .thread_id = first_thread.to_string();
    let second = workflow_task(
        root.path(),
        "wf_duplicate-residency",
        2,
        WorkflowTaskStatus::Running,
    );
    second
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .thread_id = second_thread.to_string();
    let service = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
    service.cache_task("wf_duplicate-residency".to_string(), Arc::clone(&first));
    service.cache_task("wf_duplicate-residency".to_string(), Arc::clone(&second));

    assert!(Arc::ptr_eq(
        &service
            .cached_task(first_thread, "wf_duplicate-residency")
            .unwrap(),
        &first,
    ));
    assert!(Arc::ptr_eq(
        &service
            .cached_task(second_thread, "wf_duplicate-residency")
            .unwrap(),
        &second,
    ));
    assert!(!service.keeps_thread_resident(first_thread));
    assert!(service.keeps_thread_resident(second_thread));
}

#[tokio::test]
async fn restore_keeps_duplicate_run_ids_scoped_to_their_threads() {
    let fixture = RestoreFixture::new().await;
    let run_id = "wf_duplicate-restore";
    let first = fixture
        .persist_named(
            run_id,
            WorkflowTaskStatus::Completed,
            WORKFLOW_SOURCE,
            sha256(WORKFLOW_SOURCE),
            1,
        )
        .await;
    let second_thread = ThreadId::from_string("33333333-3333-4333-8333-333333333333").unwrap();
    let session_dir = workflow_session_dir(&fixture.config.codex_home, second_thread);
    let scripts_dir = session_dir.join("workflows/scripts");
    let transcript_dir = session_dir.join("subagents/workflows").join(run_id);
    tokio::fs::create_dir_all(&scripts_dir).await.unwrap();
    tokio::fs::create_dir_all(&transcript_dir).await.unwrap();
    let script_path = scripts_dir.join(format!("{run_id}.js"));
    tokio::fs::write(&script_path, WORKFLOW_SOURCE)
        .await
        .unwrap();
    let mut second = first.clone();
    second.thread_id = second_thread.to_string();
    second.turn_id = "turn-second-thread".to_string();
    second.task_id = "wsecond-thread".to_string();
    second.script_path = script_path;
    second.transcript_dir = transcript_dir;
    second.started_at = 2;
    second.output_file = snapshot_path(&second).unwrap();
    let environment = local_environment(&fixture.config);
    write_current_snapshot(
        &second.output_file,
        &second,
        &PersistedWorkflowExecutionContext::capture(
            &fixture.config,
            second_thread,
            WorkflowEnvironmentLocation::Local,
            &[environment],
        )
        .await,
        &PersistedWorkflowComposition::empty(&validate_workflow_script(WORKFLOW_SOURCE).unwrap()),
    )
    .await
    .unwrap();

    fixture.restore().await;
    fixture
        .service
        .restore_thread(
            second_thread,
            fixture.config.clone(),
            AgentRunner::new(Weak::<ThreadManager>::new()),
        )
        .await
        .unwrap();

    assert_eq!(
        fixture
            .service
            .cached_task(fixture.thread_id, run_id)
            .unwrap()
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .thread_id,
        fixture.thread_id.to_string()
    );
    assert_eq!(
        fixture
            .service
            .cached_task(second_thread, run_id)
            .unwrap()
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .thread_id,
        second_thread.to_string()
    );
}

fn wait_service_with_task(
    status: WorkflowTaskStatus,
) -> (WorkflowService, Arc<WorkflowTask>, ThreadId) {
    let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
    let service = WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new());
    let root = AbsolutePathBuf::try_from(std::env::temp_dir())
        .unwrap()
        .join(format!("workflow-wait-test-{}", uuid::Uuid::new_v4()));
    let task = Arc::new(WorkflowTask::new(
        WorkflowTaskSnapshot {
            thread_id: thread_id.to_string(),
            turn_id: "turn-wait".to_string(),
            task_id: "wwait".to_string(),
            run_id: "wf_wait-test".to_string(),
            workflow_name: "wait-test".to_string(),
            title: None,
            status,
            summary: "wait test".to_string(),
            transcript_dir: root.join("transcript"),
            script_path: root.join("workflow.js"),
            args: JsonValue::Null,
            result_artifact: None,
            output_file: root.join("workflow.json"),
            progress: Vec::new(),
            progress_version: 0,
            usage: WorkflowUsage::default(),
            failures: Vec::new(),
            error: None,
            started_at: 1,
            completed_at: workflow_status_is_terminal(status).then_some(2),
            script_sha256: "sha256".to_string(),
        },
        test_execution_context(&root),
        PersistedWorkflowComposition::unavailable(),
    ));
    service.cache_task("wf_wait-test".to_string(), Arc::clone(&task));
    (service, task, thread_id)
}

fn delivery_snapshot(task: &Arc<WorkflowTask>) -> (tempfile::TempDir, WorkflowTaskSnapshot) {
    let directory = tempfile::tempdir().unwrap();
    let mut snapshot = task
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    snapshot.output_file =
        AbsolutePathBuf::try_from(directory.path().join("workflow.json")).unwrap();
    (directory, snapshot)
}

fn test_progress_agent(
    index: usize,
    state: WorkflowAgentState,
    error: Option<String>,
) -> codex_protocol::workflow::WorkflowAgentProgress {
    codex_protocol::workflow::WorkflowAgentProgress {
        invocation_id: format!("invocation-{index}"),
        index,
        label: format!("agent-{index}"),
        phase_index: None,
        phase_title: None,
        agent_id: None,
        model: None,
        fallback_model: None,
        isolation: None,
        state,
        activity: None,
        blocked: false,
        skipped: false,
        awaiting_decision: false,
        cached: false,
        attempt: 0,
        error,
        tokens: None,
        tool_calls: None,
        duration_ms: None,
        result_preview: None,
        prompt_preview: "test".to_string(),
        queued_at: 1,
        started_at: Some(1),
        last_progress_at: 2,
    }
}

fn test_execution_context(root: &AbsolutePathBuf) -> PersistedWorkflowExecutionContext {
    let root = PathUri::from_abs_path(root);
    PersistedWorkflowExecutionContext {
        location: PersistedWorkflowEnvironmentLocation::Local,
        selections: vec![PersistedTurnEnvironmentSelection {
            environment_id: "local".to_string(),
            cwd: root.clone(),
            workspace_roots: vec![root.clone()],
            config: PersistedEnvironmentConfigState::FromThread,
        }],
        cwd: root.clone(),
        workspace_roots: vec![root.clone()],
        permission_workspace_roots: vec![root],
        permission_identity: JsonValue::Null,
        model: Some("test-model".to_string()),
        reasoning_effort: None,
        service_tier: None,
        model_provider_id: "test-provider".to_string(),
        model_provider_fingerprint: "provider-fingerprint".to_string(),
        default_subagent_model: None,
        default_subagent_reasoning_effort: None,
        agent_roles_fingerprint: Some("roles-fingerprint".to_string()),
        approval_policy: AskForApproval::OnRequest,
        approvals_reviewer: ApprovalsReviewer::User,
        effective_config_fingerprint: "effective-config-fingerprint".to_string(),
        workspace_fingerprint: Some("workspace-fingerprint".to_string()),
        execution_environment_fingerprint: None,
    }
}
