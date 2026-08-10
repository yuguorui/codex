use super::*;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

#[tokio::test]
async fn spec_requires_run_id_without_exposing_timeout_capacity_values() {
    let mut config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    config.multi_agent_v2.min_wait_timeout_ms = 25;
    config.multi_agent_v2.max_wait_timeout_ms = 250;
    config.multi_agent_v2.default_wait_timeout_ms = 100;

    let ToolSpec::Function(spec) = wait_workflow_tool_spec(&config) else {
        panic!("WaitWorkflow should be a function tool");
    };

    assert_eq!(spec.name, WAIT_WORKFLOW_TOOL_NAME);
    assert_eq!(spec.parameters.required, Some(vec!["runId".to_string()]));
    let properties = spec.parameters.properties.unwrap();
    assert_eq!(
        properties.keys().cloned().collect::<Vec<_>>(),
        vec!["runId".to_string(), "timeoutMs".to_string(),]
    );
    let timeout_description = properties["timeoutMs"].description.as_deref().unwrap();
    assert!(timeout_description.contains("configured default"));
    assert!(!timeout_description.contains("100"));
    let output_schema = spec.output_schema.expect("WaitWorkflow output schema");
    assert_eq!(
        output_schema["required"],
        json!([
            "runId",
            "workflowName",
            "status",
            "summary",
            "error",
            "failureCount",
            "usage",
            "completedAt",
            "timedOut",
            "interruptedByUserInput",
            "timeoutMs",
            "result",
            "resultAvailable",
            "resultInline",
            "resultTruncated",
            "resultPreview",
            "resultBytes",
            "resultError",
            "nextAction"
        ])
    );
}

#[tokio::test]
async fn output_bounds_text_and_returns_the_terminal_result() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let result_chunk = crate::result_artifact::WorkflowResultChunk {
        text: "null".to_string(),
        offset: 0,
        next_offset: 4,
        total_bytes: 4,
    };
    let output = WaitWorkflowOutput::from_outcome_with_result_chunk(
        WorkflowWaitOutcome {
            snapshot: crate::service::WorkflowTaskSnapshot {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                task_id: "task".to_string(),
                run_id: "wf_test".to_string(),
                workflow_name: "test".to_string(),
                title: None,
                status: WorkflowTaskStatus::Failed,
                summary: "summary ".repeat(2_000),
                transcript_dir: root.join("transcript"),
                script_path: root.join("workflow.js"),
                args: JsonValue::Null,
                result_artifact: Some(crate::result_artifact::WorkflowResultArtifact {
                    sha256: "0".repeat(64),
                    bytes: 4,
                    storage_id: "0".repeat(32),
                }),
                output_file: root.join("workflow.json"),
                progress: Vec::new(),
                progress_version: 0,
                usage: WorkflowUsage::default(),
                failures: vec!["failure".to_string()],
                error: Some("error ".repeat(2_000)),
                started_at: 1,
                completed_at: Some(2),
                script_sha256: "sha256".to_string(),
            },
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        Some(&result_chunk),
        /*result_error*/ None,
    )
    .unwrap();

    assert_eq!(output.run_id, "wf_test");
    assert_eq!(output.failure_count, 1);
    assert!(!output.interrupted_by_user_input);
    assert!(output.summary.len() < "summary ".repeat(2_000).len());
    assert!(output.error.as_deref().unwrap().len() < "error ".repeat(2_000).len());
    let serialized = serde_json::to_value(&output).unwrap();
    assert_eq!(
        serialized,
        json!({
            "runId": "wf_test",
            "workflowName": "test",
            "status": "failed",
            "summary": output.summary,
            "error": output.error,
            "failureCount": 1,
            "usage": {
                "totalTokens": 0,
                "toolUses": 0,
                "durationMs": 0,
                "agentCount": 0
            },
            "completedAt": 2,
            "timedOut": false,
            "interruptedByUserInput": false,
            "timeoutMs": 100,
            "result": null,
            "resultAvailable": true,
            "resultInline": true,
            "resultTruncated": false,
            "resultPreview": null,
            "resultBytes": 4,
            "resultError": null,
            "nextAction": null
        })
    );
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    let ToolSpec::Function(spec) = wait_workflow_tool_spec(&config) else {
        panic!("WaitWorkflow should be a function tool");
    };
    let schema = spec.output_schema.expect("WaitWorkflow output schema");
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&serialized));
    assert!(
        serde_json::to_vec(&output).unwrap().len()
            <= crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES
    );
}

#[test]
fn low_compression_wait_output_stays_below_the_context_item_cap() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let low_compression = (0..8_000)
        .map(|index| char::from_u32(0x21 + (index * 73 % 90)).unwrap())
        .collect::<String>();
    let mut snapshot = crate::service::WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: "task".to_string(),
        run_id: "wf_low-compression".to_string(),
        workflow_name: low_compression.clone(),
        title: None,
        status: WorkflowTaskStatus::Failed,
        summary: low_compression.clone(),
        transcript_dir: root.join("transcript"),
        script_path: root.join("workflow.js"),
        args: JsonValue::Null,
        result_artifact: Some(crate::result_artifact::WorkflowResultArtifact {
            sha256: "0".repeat(64),
            bytes: 4,
            storage_id: "0".repeat(32),
        }),
        output_file: root.join(low_compression),
        progress: Vec::new(),
        progress_version: 0,
        usage: WorkflowUsage::default(),
        failures: Vec::new(),
        error: Some("!@#$%^&*()[]{}<>?/|\\".repeat(400)),
        started_at: 1,
        completed_at: Some(2),
        script_sha256: "sha256".to_string(),
    };
    snapshot.output_file = root.join("workflow.json");
    let output = WaitWorkflowOutput::from_outcome_with_result_chunk(
        WorkflowWaitOutcome {
            snapshot,
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
        /*result_chunk*/ None,
        Some("corrupt ".repeat(1_000).as_str()),
    )
    .unwrap();

    let serialized_bytes = serde_json::to_vec(&output).unwrap().len();
    let result_data = serde_json::to_value(&output.result_data).unwrap();
    assert_eq!(result_data["resultAvailable"], false);
    assert!(result_data["resultError"].as_str().is_some());
    assert_eq!(
        result_data["nextAction"],
        "Call ReadWorkflowResult with runId \"wf_low-compression\" and offset 0."
    );
    assert!(!result_data["nextAction"].as_str().unwrap().contains('/'));
    assert!(serialized_bytes < 1_000);
    assert!(serialized_bytes <= crate::workflow_result_tool::WAIT_MODEL_CONTEXT_ITEM_MAX_BYTES);
}

#[test]
fn interrupted_wait_is_not_reported_as_a_timeout() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let output = WaitWorkflowOutput::from_outcome(
        WorkflowWaitOutcome {
            snapshot: crate::service::WorkflowTaskSnapshot {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                task_id: "task".to_string(),
                run_id: "wf_interrupted".to_string(),
                workflow_name: "test".to_string(),
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
            timed_out: true,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ true,
    )
    .unwrap();

    assert!(output.interrupted_by_user_input);
    assert!(!output.timed_out);
    assert_eq!(output.status, WorkflowTaskStatus::Running);
}

#[test]
fn paused_wait_is_terminal_without_exposing_a_result() {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    let mut snapshot = crate::service::WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: "task".to_string(),
        run_id: "wf_paused".to_string(),
        workflow_name: "test".to_string(),
        title: None,
        status: WorkflowTaskStatus::Paused,
        summary: "paused".to_string(),
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
        completed_at: Some(2),
        script_sha256: "sha256".to_string(),
    };
    let output = WaitWorkflowOutput::from_outcome(
        WorkflowWaitOutcome {
            snapshot: snapshot.clone(),
            timed_out: false,
        },
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
    )
    .unwrap();

    assert_eq!(output.status, WorkflowTaskStatus::Paused);
    assert!(!output.timed_out);
    let result_data = serde_json::to_value(&output.result_data).unwrap();
    assert_eq!(result_data["resultAvailable"], false);
    assert_eq!(result_data["resultInline"], false);
    snapshot.status = WorkflowTaskStatus::Completed;
    snapshot.result_artifact = Some(crate::result_artifact::WorkflowResultArtifact {
        sha256: "0".repeat(64),
        bytes: 4,
        storage_id: "0".repeat(32),
    });
    let result_chunk = crate::result_artifact::WorkflowResultChunk {
        text: "null".to_string(),
        offset: 0,
        next_offset: 4,
        total_bytes: 4,
    };
    let completed_result_data = serde_json::to_value(
        WaitWorkflowOutput::from_outcome_with_result_chunk(
            WorkflowWaitOutcome {
                snapshot,
                timed_out: false,
            },
            /*timeout_ms*/ 100,
            /*interrupted_by_user_input*/ false,
            Some(&result_chunk),
            /*result_error*/ None,
        )
        .unwrap()
        .result_data,
    )
    .unwrap();
    assert_eq!(completed_result_data["resultAvailable"], true);
}

#[test]
fn parse_errors_are_bounded_before_reaching_the_model() {
    let arguments = format!(r#"{{"{}":true}}"#, "unknown".repeat(2_000));

    let Err(FunctionCallError::RespondToModel(message)) = parse_arguments(&arguments) else {
        panic!("oversized unknown field should produce a model-visible parse error");
    };

    assert!(message.len() <= crate::workflow_result_tool::MODEL_ERROR_MAX_BYTES);
    assert!(message.ends_with("...[truncated]"));
}

#[tokio::test]
async fn repeated_waits_observe_the_same_latched_user_input() {
    let activity = Arc::new(LatchedTurnActivity::default());
    activity.signal_user_input();

    for _ in 0..2 {
        let result = race_with_turn_activity(
            std::future::pending::<()>(),
            Some(activity.clone() as Arc<dyn TurnActivitySubscription>),
        )
        .await;
        assert!(matches!(result, InterruptibleWait::InterruptedByUserInput));
    }
}

#[tokio::test]
async fn user_input_wakes_an_already_running_wait() {
    let activity = Arc::new(LatchedTurnActivity::default());
    let wait_activity = Arc::clone(&activity);
    let wait = tokio::spawn(async move {
        race_with_turn_activity(
            std::future::pending::<()>(),
            Some(wait_activity as Arc<dyn TurnActivitySubscription>),
        )
        .await
    });
    timeout(Duration::from_secs(1), activity.wait_started.notified())
        .await
        .expect("wait should subscribe to turn activity");
    assert!(!wait.is_finished());

    activity.signal_user_input();

    assert!(matches!(
        timeout(Duration::from_secs(1), wait)
            .await
            .expect("user input should wake the active wait")
            .expect("wait task should complete"),
        InterruptibleWait::InterruptedByUserInput
    ));
}

#[derive(Default)]
struct LatchedTurnActivity {
    observed: AtomicBool,
    notify: Notify,
    wait_started: Notify,
}

impl LatchedTurnActivity {
    fn signal_user_input(&self) {
        self.observed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

impl TurnActivitySubscription for LatchedTurnActivity {
    fn observed(&self) -> Option<TurnActivity> {
        self.observed
            .load(Ordering::Acquire)
            .then_some(TurnActivity::UserInput)
    }

    fn wait<'a>(&'a self) -> codex_extension_api::TurnActivityFuture<'a> {
        Box::pin(async move {
            self.wait_started.notify_one();
            if !self.observed.load(Ordering::Acquire) {
                self.notify.notified().await;
            }
            self.observed()
        })
    }
}
