use super::*;
use crate::agent::WorkflowEnvironmentLocation;
use crate::discovery::ResolvedWorkflow;
use crate::discovery::WorkflowOrigin;
use crate::service::WorkflowLaunchRequest;
use codex_agent_extension::AgentRunner;
use codex_config::LoaderOverrides;
use codex_core::ThreadManager;
use codex_core::config::ConfigBuilder;
use codex_extension_api::ConversationHistory;
use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::NoopExtensionEventSink;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolPayload;
use codex_extension_api::TurnActivity;
use codex_extension_api::TurnActivityFuture;
use codex_extension_api::TurnActivitySubscription;
use codex_extension_api::TurnItemEmissionFuture;
use codex_extension_api::TurnItemEmitter;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_workflow::validate_workflow_script;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::sync::Arc;
use std::sync::Weak;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio::time::timeout;

fn workflow_status_is_terminal(status: WorkflowTaskStatus) -> bool {
    matches!(
        status,
        WorkflowTaskStatus::Completed
            | WorkflowTaskStatus::Failed
            | WorkflowTaskStatus::Paused
            | WorkflowTaskStatus::Killed
    )
}

#[test]
fn list_is_filtered_and_hard_limited() {
    let snapshots = vec![
        snapshot("wf_new", /*started_at*/ 3, WorkflowTaskStatus::Running),
        snapshot(
            "wf_middle",
            /*started_at*/ 2,
            WorkflowTaskStatus::Completed,
        ),
        snapshot(
            "wf_old",
            /*started_at*/ 1,
            WorkflowTaskStatus::Completed,
        ),
    ];

    let output = list_workflows_output(
        snapshots,
        ListWorkflowsArgs {
            limit: Some(1),
            statuses: Some(vec![WorkflowTaskStatus::Completed]),
            cursor: None,
        },
    )
    .unwrap();

    assert_eq!(output.workflows.len(), 1);
    assert_eq!(output.workflows[0].run_id, "wf_middle");
    assert_eq!(output.total_matched, 2);
    assert!(output.truncated);
    assert!(output.next_cursor.is_some());
}

#[test]
fn list_cursor_pages_stably_without_duplicates_or_omissions() {
    let snapshot_count = crate::service::MAX_RETAINED_TERMINAL_TASKS + 3;
    let snapshots = (0..snapshot_count)
        .rev()
        .map(|index| {
            snapshot(
                &format!("wf_{index:03}"),
                i64::try_from(index / 3).unwrap(),
                WorkflowTaskStatus::Completed,
            )
        })
        .collect::<Vec<_>>();
    let mut cursor = None;
    let mut run_ids = Vec::new();

    loop {
        let output = list_workflows_output(
            snapshots.clone(),
            ListWorkflowsArgs {
                limit: Some(17),
                statuses: Some(vec![WorkflowTaskStatus::Completed]),
                cursor,
            },
        )
        .unwrap();
        run_ids.extend(output.workflows.into_iter().map(|workflow| workflow.run_id));
        let Some(next_cursor) = output.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }

    let mut expected = snapshots;
    expected.sort_by(|left, right| {
        right
            .started_at
            .cmp(&left.started_at)
            .then_with(|| right.run_id.cmp(&left.run_id))
    });
    assert_eq!(
        run_ids,
        expected
            .into_iter()
            .map(|snapshot| snapshot.run_id)
            .collect::<Vec<_>>()
    );
}

#[test]
fn list_rejects_an_invalid_cursor() {
    let error = list_workflows_output(
        Vec::new(),
        ListWorkflowsArgs {
            limit: None,
            statuses: None,
            cursor: Some("not-a-cursor".to_string()),
        },
    )
    .unwrap_err();

    assert_eq!(error, "invalid workflow list cursor");
}

#[test]
fn list_cursor_binds_new_tokens_to_their_statuses_filter() {
    let snapshots = vec![
        snapshot(
            "wf_running",
            /*started_at*/ 3,
            WorkflowTaskStatus::Running,
        ),
        snapshot(
            "wf_completed",
            /*started_at*/ 2,
            WorkflowTaskStatus::Completed,
        ),
        snapshot(
            "wf_completed_old",
            /*started_at*/ 1,
            WorkflowTaskStatus::Completed,
        ),
    ];
    let output = list_workflows_output(
        snapshots.clone(),
        ListWorkflowsArgs {
            limit: Some(1),
            statuses: Some(vec![WorkflowTaskStatus::Completed]),
            cursor: None,
        },
    )
    .unwrap();
    let cursor = output.next_cursor.expect("filtered list should continue");
    assert_eq!(
        decode_list_cursor(&cursor).unwrap().statuses,
        Some(vec![WorkflowTaskStatus::Completed])
    );

    let error = list_workflows_output(
        snapshots.clone(),
        ListWorkflowsArgs {
            limit: Some(1),
            statuses: Some(vec![WorkflowTaskStatus::Running]),
            cursor: Some(cursor.clone()),
        },
    )
    .unwrap_err();
    assert_eq!(error, "list cursor belongs to a different statuses filter");

    list_workflows_output(
        snapshots.clone(),
        ListWorkflowsArgs {
            limit: Some(1),
            statuses: Some(vec![
                WorkflowTaskStatus::Completed,
                WorkflowTaskStatus::Completed,
            ]),
            cursor: Some(cursor),
        },
    )
    .unwrap();

    let two_status = list_workflows_output(
        snapshots.clone(),
        ListWorkflowsArgs {
            limit: Some(1),
            statuses: Some(vec![
                WorkflowTaskStatus::Completed,
                WorkflowTaskStatus::Running,
            ]),
            cursor: None,
        },
    )
    .unwrap();
    let two_status_cursor = two_status
        .next_cursor
        .expect("two-status list should continue");
    list_workflows_output(
        snapshots.clone(),
        ListWorkflowsArgs {
            limit: Some(1),
            statuses: Some(vec![
                WorkflowTaskStatus::Running,
                WorkflowTaskStatus::Completed,
            ]),
            cursor: Some(two_status_cursor),
        },
    )
    .unwrap();

    let omitted = list_workflows_output(
        snapshots,
        ListWorkflowsArgs {
            limit: Some(1),
            statuses: None,
            cursor: None,
        },
    )
    .unwrap();
    let full_cursor = omitted
        .next_cursor
        .expect("unfiltered list should continue");
    list_workflows_output(
        Vec::new(),
        ListWorkflowsArgs {
            limit: Some(1),
            statuses: Some(WORKFLOW_STATUSES.to_vec()),
            cursor: Some(full_cursor),
        },
    )
    .unwrap();

    let legacy_cursor = serde_json::to_string(&json!({"sequence": 1})).unwrap();
    list_workflows_output(
        Vec::new(),
        ListWorkflowsArgs {
            limit: Some(1),
            statuses: Some(vec![WorkflowTaskStatus::Completed]),
            cursor: Some(legacy_cursor),
        },
    )
    .unwrap();

    let explicit_all = WORKFLOW_STATUSES.to_vec();
    assert_eq!(
        canonical_statuses(&explicit_all),
        Vec::<WorkflowTaskStatus>::new()
    );
    assert_eq!(
        canonical_statuses(&[
            WorkflowTaskStatus::Killed,
            WorkflowTaskStatus::Failed,
            WorkflowTaskStatus::Killed,
        ]),
        vec![WorkflowTaskStatus::Failed, WorkflowTaskStatus::Killed]
    );
}

#[test]
fn collections_reject_oversized_list_limits() {
    assert!(
        list_workflows_output(
            Vec::new(),
            ListWorkflowsArgs {
                limit: Some(MAX_WORKFLOW_COLLECTION_ITEMS + 1),
                statuses: None,
                cursor: None,
            },
        )
        .is_err()
    );
}

#[test]
fn wait_output_preserves_input_order_and_mode_condition() {
    let outcomes = vec![
        WorkflowWaitOutcome {
            snapshot: snapshot("wf_1", /*started_at*/ 2, WorkflowTaskStatus::Completed),
            timed_out: false,
        },
        WorkflowWaitOutcome {
            snapshot: snapshot("wf_2", /*started_at*/ 1, WorkflowTaskStatus::Running),
            timed_out: true,
        },
    ];

    let all = wait_workflows_output(
        WaitMode::All,
        &outcomes,
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
    );
    let any = wait_workflows_output(
        WaitMode::Any,
        &outcomes,
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
    );

    assert!(!all.condition_met);
    assert!(all.timed_out);
    assert_eq!(
        all.workflows
            .iter()
            .map(|workflow| workflow.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["wf_1", "wf_2"]
    );
    assert!(any.condition_met);
    assert!(!any.timed_out);
    assert!(!any.interrupted_by_user_input);
}

#[tokio::test(start_paused = true)]
async fn wait_all_uses_one_shared_deadline_and_preserves_input_order() {
    let fixture = WaitFixture::new().await;
    let first_run_id = fixture.launch_pending("shared-deadline-first").await;
    let second_run_id = fixture.launch_pending("shared-deadline-second").await;
    let timeout_duration = Duration::from_millis(250);
    let started = Instant::now();

    let output = wait_for_workflows(
        fixture.service.clone(),
        fixture.thread_id,
        vec![second_run_id.clone(), first_run_id.clone()],
        WaitMode::All,
        timeout_duration,
        /*timeout_ms*/ 250,
    )
    .await
    .unwrap();

    assert_eq!(Instant::now().duration_since(started), timeout_duration);
    assert_eq!(
        output,
        WaitWorkflowsOutput {
            mode: WaitMode::All,
            condition_met: false,
            timed_out: true,
            interrupted_by_user_input: false,
            timeout_ms: 250,
            workflows: vec![
                WaitedWorkflowStatus {
                    run_id: second_run_id.clone(),
                    status: WorkflowTaskStatus::Running,
                    timed_out: true,
                    result_available: false,
                    result_bytes: None,
                    result_sha256: None,
                    recovery: waited_recovery("running", false),
                },
                WaitedWorkflowStatus {
                    run_id: first_run_id.clone(),
                    status: WorkflowTaskStatus::Running,
                    timed_out: true,
                    result_available: false,
                    result_bytes: None,
                    result_sha256: None,
                    recovery: waited_recovery("running", false),
                },
            ],
            winner: None,
        }
    );

    fixture
        .service
        .stop(fixture.thread_id, &first_run_id)
        .await
        .unwrap();
    fixture
        .service
        .stop(fixture.thread_id, &second_run_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn wait_any_returns_the_first_terminal_run_without_waiting_for_siblings() {
    let fixture = WaitFixture::new().await;
    let first_run_id = fixture.launch_pending("wait-any-first").await;
    let second_run_id = fixture.launch_pending("wait-any-second").await;
    let wait_run_ids = vec![first_run_id.clone(), second_run_id.clone()];
    let thread_id = fixture.thread_id;
    let waiter = tokio::spawn({
        let service = fixture.service.clone();
        async move {
            wait_for_workflows(
                service,
                thread_id,
                wait_run_ids,
                WaitMode::Any,
                Duration::from_secs(5),
                /*timeout_ms*/ 5_000,
            )
            .await
        }
    });
    tokio::task::yield_now().await;
    fixture
        .service
        .stop(fixture.thread_id, &second_run_id)
        .await
        .unwrap();

    let output = waiter.await.unwrap().unwrap();

    // Every requested run stays visible in request order, including the sibling
    // that `mode: any` stopped waiting for.
    assert_eq!(output.mode, WaitMode::Any);
    assert!(output.condition_met);
    assert!(!output.timed_out);
    assert!(!output.interrupted_by_user_input);
    assert_eq!(output.timeout_ms, 5_000);
    assert_eq!(
        output.workflows,
        vec![
            WaitedWorkflowStatus {
                run_id: first_run_id.clone(),
                status: WorkflowTaskStatus::Running,
                timed_out: true,
                result_available: false,
                result_bytes: None,
                result_sha256: None,
                recovery: waited_recovery("running", false),
            },
            WaitedWorkflowStatus {
                run_id: second_run_id.clone(),
                status: WorkflowTaskStatus::Killed,
                timed_out: false,
                result_available: true,
                result_bytes: Some(4),
                result_sha256: Some(
                    "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b".to_string(),
                ),
                recovery: waited_recovery("killed", true),
            },
        ]
    );

    // The winner carries the same bounded result head WaitWorkflow would return.
    // Usage is asserted field-wise because durationMs is wall-clock derived.
    let Some(winner) = output.winner.clone() else {
        panic!("mode any should report the run that satisfied the condition");
    };
    assert_eq!(winner.run_id, second_run_id);
    assert_eq!(winner.workflow_name, "wait-any-second");
    assert_eq!(winner.status, WorkflowTaskStatus::Killed);
    assert_eq!(winner.summary, "Workflow wait-any-second stopped");
    assert_eq!(winner.error, None);
    assert_eq!(winner.usage.total_tokens, 0);
    assert_eq!(winner.usage.tool_uses, 0);
    let result_data = serde_json::to_value(&winner.result_data).unwrap();
    assert_eq!(result_data["resultAvailable"], json!(true));
    assert_eq!(result_data["resultInline"], json!(true));
    assert_eq!(result_data["resultTruncated"], json!(false));
    assert_eq!(result_data["resultBytes"], json!(4));
    assert_eq!(result_data["resultError"], json!(null));
    assert_eq!(result_data["nextAction"], json!(null));

    fixture
        .service
        .stop(fixture.thread_id, &first_run_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn wait_all_ignores_non_user_activity_after_first_completion() {
    let fixture = WaitFixture::new().await;
    let first_run_id = fixture.launch_pending("notification-first").await;
    let second_run_id = fixture.launch_pending("notification-second").await;
    let activity = Arc::new(ControlledTurnActivity::default());
    let payload = ToolPayload::Function {
        arguments: json!({
            "runIds": [&first_run_id, &second_run_id],
            "mode": "all",
            "timeoutMs": 5_000,
        })
        .to_string(),
    };
    let output_payload = payload.clone();
    let executor = WaitWorkflowsToolExecutor::new(
        fixture.thread_id,
        fixture.config.clone(),
        fixture.service.clone(),
    );
    let emitter = Arc::new(ActivityEmitter {
        activity: Arc::clone(&activity),
    });
    let wait = tokio::spawn(async move {
        executor
            .handle(ToolCall {
                turn_id: "turn-notification-wait".to_string(),
                call_id: "call-notification-wait".to_string(),
                tool_name: ToolName::plain(WAIT_WORKFLOWS_TOOL_NAME),
                model: "test-model".to_string(),
                codex_turn_metadata: None,
                truncation_policy: TruncationPolicy::Bytes(1_024),
                source: ToolCallSource::Direct,
                conversation_history: ConversationHistory::default(),
                turn_item_emitter: emitter,
                environments: Vec::new(),
                agent_configuration: None,
                payload,
            })
            .await
    });

    timeout(Duration::from_secs(1), activity.wait_entered.notified())
        .await
        .expect("WaitWorkflows should enter its activity subscription");
    fixture
        .service
        .stop(fixture.thread_id, &first_run_id)
        .await
        .unwrap();
    let first = fixture
        .service
        .wait_for_terminal(fixture.thread_id, &first_run_id, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(first.snapshot.status, WorkflowTaskStatus::Killed);

    activity.signal_non_user_wake();
    timeout(
        Duration::from_secs(1),
        activity.non_user_wake_processed.notified(),
    )
    .await
    .expect("activity subscription should process the non-user wake");
    assert!(!wait.is_finished());

    fixture
        .service
        .stop(fixture.thread_id, &second_run_id)
        .await
        .unwrap();
    let output = timeout(Duration::from_secs(5), wait)
        .await
        .expect("WaitWorkflows should finish after the second completion")
        .expect("WaitWorkflows task should not panic")
        .expect("WaitWorkflows should succeed");
    let value = output.code_mode_result(&output_payload);
    assert_eq!(value["conditionMet"], true);
    assert_eq!(value["timedOut"], false);
    assert_eq!(value["interruptedByUserInput"], false);
    assert_eq!(
        value["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|workflow| (
                workflow["runId"].as_str().unwrap(),
                workflow["status"].as_str().unwrap(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (first_run_id.as_str(), "killed"),
            (second_run_id.as_str(), "killed"),
        ]
    );
}

#[test]
fn interrupted_multi_wait_preserves_all_run_statuses_without_timeout() {
    let outcomes = vec![
        WorkflowWaitOutcome {
            snapshot: snapshot("wf_1", /*started_at*/ 2, WorkflowTaskStatus::Running),
            timed_out: true,
        },
        WorkflowWaitOutcome {
            snapshot: snapshot("wf_2", /*started_at*/ 1, WorkflowTaskStatus::Running),
            timed_out: true,
        },
    ];

    let output = wait_workflows_output(
        WaitMode::All,
        &outcomes,
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ true,
    );

    assert!(!output.condition_met);
    assert!(!output.timed_out);
    assert!(output.interrupted_by_user_input);
    assert!(output.workflows.iter().all(|workflow| !workflow.timed_out));
    assert_eq!(
        output
            .workflows
            .iter()
            .map(|workflow| workflow.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["wf_1", "wf_2"]
    );
}

#[test]
fn paused_is_terminal_for_waits_without_exposing_a_result() {
    let paused = snapshot(
        "wf_paused",
        /*started_at*/ 1,
        WorkflowTaskStatus::Paused,
    );
    let status = WorkflowStatusItem::from_snapshot(&paused);
    let output = wait_workflows_output(
        WaitMode::All,
        &[WorkflowWaitOutcome {
            snapshot: paused,
            timed_out: false,
        }],
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
    );

    assert!(!status.result_available);
    assert!(output.condition_met);
    assert!(!output.timed_out);
    assert_eq!(output.workflows.len(), 1);
    assert_eq!(output.workflows[0].status, WorkflowTaskStatus::Paused);
    assert!(!output.workflows[0].result_available);
}

#[test]
fn maximum_multi_workflow_outputs_stay_within_the_hard_bound() {
    let outcomes = (0..MAX_WAIT_WORKFLOW_ITEMS)
        .map(|index| {
            // Production run ids are `wf_` plus 32 hex characters; short fixture ids
            // under-measure the batch by more than 200 bytes.
            let mut snapshot = snapshot(
                &format!("wf_{index:032x}"),
                i64::try_from(index).unwrap(),
                WorkflowTaskStatus::Killed,
            );
            snapshot.result_artifact = Some(crate::result_artifact::WorkflowResultArtifact {
                sha256: "0".repeat(64),
                bytes: 1_234,
                storage_id: "1".repeat(32),
            });
            snapshot.error = Some(
                "script content changed; workflow arguments changed; declared inputs changed; workflow execution identity changed; failed to restore frozen child workflow composition"
                    .to_string(),
            );
            WorkflowWaitOutcome {
                snapshot,
                timed_out: true,
            }
        })
        .collect::<Vec<_>>();

    let output = wait_workflows_output(
        WaitMode::All,
        &outcomes,
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
    );

    assert_eq!(output.workflows.len(), MAX_WAIT_WORKFLOW_ITEMS);
    assert!(
        output
            .workflows
            .iter()
            .all(|workflow| workflow.recovery.as_ref().is_some())
    );
    assert!(output.workflows.iter().all(|workflow| {
        workflow.result_available
            && workflow.result_bytes == Some(1_234)
            && workflow.result_sha256.as_deref() == Some("0".repeat(64).as_str())
            && workflow.recovery.as_ref().is_some_and(|recovery| {
                serde_json::to_value(recovery).unwrap()["observedRestoreIncompatibilities"]
                    .as_array()
                    .is_some_and(|incompatibilities| incompatibilities.len() == 5)
            })
    }));
    let entries_only_bytes = serde_json::to_vec(&output).unwrap().len();
    assert!(
        entries_only_bytes <= MODEL_TOOL_OUTPUT_MAX_BYTES,
        "a full batch on its own must fit the hard cap: {entries_only_bytes} bytes"
    );

    // Worst case for the whole response: a full batch of heavy entries plus a
    // `mode: any` winner carrying oversized name/summary/error text and a failed
    // inline result read.
    let mut winner_snapshot = snapshot(
        "wf_ffffffffffffffffffffffffffffffff",
        9,
        WorkflowTaskStatus::Failed,
    );
    winner_snapshot.workflow_name = "n".repeat(4_000);
    winner_snapshot.summary = "s".repeat(4_000);
    winner_snapshot.error = Some("e".repeat(4_000));
    winner_snapshot.result_artifact = Some(crate::result_artifact::WorkflowResultArtifact {
        sha256: "0".repeat(64),
        bytes: 64_000,
        storage_id: "1".repeat(32),
    });
    let winner_text = WaitRunText::from_snapshot(&winner_snapshot);
    let with_winner = WaitWorkflowsOutput {
        mode: WaitMode::Any,
        condition_met: true,
        winner: Some(WaitWinnerResult {
            run_id: winner_snapshot.run_id.clone(),
            workflow_name: winner_text.workflow_name,
            status: winner_snapshot.status,
            summary: winner_text.summary,
            error: winner_text.error,
            usage: winner_snapshot.usage.clone(),
            result_data: WorkflowResultData::without_chunk(
                &winner_snapshot,
                Some(&"read failure ".repeat(400)),
            ),
        }),
        ..output.clone()
    };
    // A full batch plus a heavy winner exceeds the hard cap, so the bounding ladder
    // has to reclaim it: compact the inline head first, then drop the winner. The
    // per-run statuses are never sacrificed.
    let with_winner_bytes = serde_json::to_vec(&with_winner).unwrap().len();
    assert!(
        with_winner_bytes > MODEL_TOOL_OUTPUT_MAX_BYTES,
        "this case should exercise the bounding ladder: {with_winner_bytes} bytes"
    );
    let mut bounded = with_winner.clone();
    bound_wait_workflows_output(&mut bounded);
    let bounded_bytes = serde_json::to_vec(&bounded).unwrap().len();
    assert!(
        bounded_bytes <= MODEL_TOOL_OUTPUT_MAX_BYTES,
        "bounded response must fit the hard cap: {bounded_bytes} bytes"
    );
    assert!(bounded.condition_met);
    // Run statuses are the contract and survive every rung.
    assert_eq!(bounded.workflows.len(), MAX_WAIT_WORKFLOW_ITEMS);
    assert_eq!(
        bounded
            .workflows
            .iter()
            .map(|workflow| (&workflow.run_id, workflow.status, workflow.result_bytes))
            .collect::<Vec<_>>(),
        with_winner
            .workflows
            .iter()
            .map(|workflow| (&workflow.run_id, workflow.status, workflow.result_bytes))
            .collect::<Vec<_>>()
    );
    // Advisory per-entry restore detail is spent before the winner is given up, and
    // on this batch it is still not enough, so the winner goes last.
    assert!(
        bounded
            .workflows
            .iter()
            .all(
                |workflow| workflow.recovery.as_ref().is_some_and(|recovery| {
                    serde_json::to_value(recovery).unwrap()["observedRestoreIncompatibilities"]
                        .as_array()
                        .is_some_and(Vec::is_empty)
                })
            )
    );
    assert!(bounded.winner.is_none());

    // A lighter winner is saved by the per-entry rung alone, so the model keeps the
    // inline head it actually wanted.
    let light_winner_snapshot = snapshot(
        "wf_eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        9,
        WorkflowTaskStatus::Completed,
    );
    let light_text = WaitRunText::from_snapshot(&light_winner_snapshot);
    let mut with_light_winner = WaitWorkflowsOutput {
        winner: Some(WaitWinnerResult {
            run_id: light_winner_snapshot.run_id.clone(),
            workflow_name: light_text.workflow_name,
            status: light_winner_snapshot.status,
            summary: light_text.summary,
            error: light_text.error,
            usage: light_winner_snapshot.usage.clone(),
            result_data: WorkflowResultData::without_chunk(&light_winner_snapshot, None),
        }),
        ..with_winner.clone()
    };
    assert!(serde_json::to_vec(&with_light_winner).unwrap().len() > MODEL_TOOL_OUTPUT_MAX_BYTES);
    bound_wait_workflows_output(&mut with_light_winner);
    assert!(serde_json::to_vec(&with_light_winner).unwrap().len() <= MODEL_TOOL_OUTPUT_MAX_BYTES);
    assert!(
        with_light_winner.winner.is_some(),
        "spending per-entry advisory detail should be enough to keep a light winner"
    );
    assert_eq!(
        with_light_winner
            .winner
            .as_ref()
            .map(|winner| &winner.run_id),
        Some(&light_winner_snapshot.run_id)
    );

    // A response that already fits is left untouched, so the inline head survives.
    let mut fitting = WaitWorkflowsOutput {
        mode: WaitMode::Any,
        condition_met: true,
        workflows: vec![output.workflows[0].clone()],
        winner: with_winner.winner.clone(),
        ..output.clone()
    };
    let before_bounding = fitting.clone();
    bound_wait_workflows_output(&mut fitting);
    assert_eq!(fitting, before_bounding);
    assert!(fitting.winner.is_some());

    let ToolSpec::Function(spec) = wait_workflows_tool_spec() else {
        panic!("WaitWorkflows should be a function tool");
    };
    let schema = spec.output_schema.expect("WaitWorkflows output schema");
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&serde_json::to_value(output).unwrap()));
    assert!(
        validator.is_valid(&serde_json::to_value(with_winner).unwrap()),
        "winner payload must validate against the declared output schema"
    );
    assert!(
        validator.is_valid(&serde_json::to_value(bounded).unwrap()),
        "a dropped winner must still validate against the declared output schema"
    );
}

#[tokio::test]
async fn specs_describe_server_sized_pages_without_capacity_values() {
    let mut config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    config.multi_agent_v2.default_wait_timeout_ms = 123;

    let ToolSpec::Function(list_spec) = list_workflows_tool_spec() else {
        panic!("ListWorkflows should be a function tool");
    };
    let ToolSpec::Function(agent_list_spec) = list_workflow_agents_tool_spec() else {
        panic!("ListWorkflowAgents should be a function tool");
    };
    let ToolSpec::Function(wait_spec) = wait_workflows_tool_spec() else {
        panic!("WaitWorkflows should be a function tool");
    };

    assert_eq!(list_spec.name, LIST_WORKFLOWS_TOOL_NAME);
    assert!(list_spec.output_schema.is_some());
    let list_properties = list_spec.parameters.properties.unwrap();
    let limit_description = list_properties["limit"].description.as_deref().unwrap();
    assert!(limit_description.contains("server-sized list page"));
    assert!(!limit_description.contains(&MAX_WORKFLOW_COLLECTION_ITEMS.to_string()));
    assert_eq!(agent_list_spec.name, LIST_WORKFLOW_AGENTS_TOOL_NAME);
    assert_eq!(
        agent_list_spec.parameters.required,
        Some(vec!["runId".to_string()])
    );
    assert_eq!(wait_spec.name, WAIT_WORKFLOWS_TOOL_NAME);
    assert_eq!(
        wait_spec.parameters.required,
        Some(vec!["runIds".to_string()])
    );
    let wait_properties = wait_spec.parameters.properties.unwrap();
    let timeout_description = wait_properties["timeoutMs"].description.as_deref().unwrap();
    assert!(timeout_description.contains("configured default"));
    assert!(!timeout_description.contains("123"));
}

#[test]
fn status_metadata_exposes_verified_result_size_and_digest() {
    let mut completed = snapshot(
        "wf_result",
        /*started_at*/ 1,
        WorkflowTaskStatus::Completed,
    );
    completed.result_artifact = Some(crate::result_artifact::WorkflowResultArtifact {
        sha256: "0".repeat(64),
        bytes: 1234,
        storage_id: "1".repeat(32),
    });

    let status = WorkflowStatusItem::from_snapshot(&completed);
    let waited = wait_workflows_output(
        WaitMode::All,
        &[WorkflowWaitOutcome {
            snapshot: completed,
            timed_out: false,
        }],
        /*timeout_ms*/ 100,
        /*interrupted_by_user_input*/ false,
    );

    assert_eq!(status.result_available, true);
    assert_eq!(status.result_bytes, Some(1234));
    assert_eq!(
        status.result_sha256.as_deref(),
        Some("0".repeat(64).as_str())
    );
    assert_eq!(waited.workflows[0].result_bytes, Some(1234));
    assert_eq!(
        waited.workflows[0].result_sha256.as_deref(),
        Some("0".repeat(64).as_str())
    );

    let missing_artifact = snapshot("wf_missing_result", 2, WorkflowTaskStatus::Completed);
    let missing = WorkflowStatusItem::from_snapshot(&missing_artifact);
    assert_eq!(missing.result_available, false);
    assert_eq!(missing.result_bytes, None);
    assert_eq!(missing.result_sha256, None);
}

#[test]
fn status_text_is_bounded() {
    let mut snapshot = snapshot("wf_long", /*started_at*/ 1, WorkflowTaskStatus::Failed);
    snapshot.summary = "summary ".repeat(1_000);
    snapshot.error = Some("error ".repeat(1_000));

    let status = WorkflowStatusItem::from_snapshot(&snapshot);

    assert!(status.summary.len() < snapshot.summary.len());
    assert!(status.error.unwrap().len() < snapshot.error.unwrap().len());
}

#[test]
fn workflow_agent_status_exposes_real_agent_and_invocation_ids() {
    let status = WorkflowAgentStatus::from(WorkflowAgentProgress {
        invocation_id: "phase/inspect/0".to_string(),
        index: 3,
        label: "inspect".to_string(),
        phase_index: Some(1),
        phase_title: Some("Inspect".to_string()),
        agent_id: Some("0198-agent-id".to_string()),
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
        tokens: Some(42),
        tool_calls: Some(2),
        duration_ms: Some(100),
        result_preview: None,
        prompt_preview: String::new(),
        queued_at: 1,
        started_at: Some(2),
        last_progress_at: 3,
    });

    assert_eq!(
        status,
        WorkflowAgentStatus {
            invocation_id: "phase/inspect/0".to_string(),
            index: 3,
            agent_id: Some("0198-agent-id".to_string()),
            label: "inspect".to_string(),
            phase_index: Some(1),
            phase_title: Some("Inspect".to_string()),
            state: WorkflowAgentState::Done,
            blocked: false,
            skipped: false,
            awaiting_decision: false,
            cached: false,
            attempt: 0,
            error: None,
            tokens: Some(42),
            tool_calls: Some(2),
            duration_ms: Some(100),
        }
    );
}

#[test]
fn list_truncates_the_collection_before_the_model_output_limit() {
    let snapshots = (0..MAX_WORKFLOW_COLLECTION_ITEMS)
        .map(|index| {
            // Production run ids are `wf_` plus 32 hex characters; short fixture ids
            // under-measure the batch by more than 200 bytes.
            let mut snapshot = snapshot(
                &format!("wf_{index:032x}"),
                i64::try_from(index).unwrap(),
                WorkflowTaskStatus::Failed,
            );
            snapshot.workflow_name = "\u{0000}\"\\".repeat(200);
            snapshot.title = Some("\u{0000}\"\\".repeat(200));
            snapshot.summary = "\u{0000}\"\\".repeat(200);
            snapshot.error = Some("\u{0000}\"\\".repeat(200));
            snapshot
        })
        .collect();

    let output = list_workflows_output(
        snapshots,
        ListWorkflowsArgs {
            limit: Some(MAX_WORKFLOW_COLLECTION_ITEMS),
            statuses: None,
            cursor: None,
        },
    )
    .unwrap();

    assert!(output.truncated);
    assert!(serde_json::to_vec(&output).unwrap().len() <= MODEL_TOOL_OUTPUT_MAX_BYTES);
    let ToolSpec::Function(spec) = list_workflows_tool_spec() else {
        panic!("ListWorkflows should be a function tool");
    };
    let schema = spec.output_schema.expect("ListWorkflows output schema");
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&serde_json::to_value(output).unwrap()));
}

#[test]
fn list_and_wait_parse_errors_are_bounded_before_reaching_the_model() {
    let arguments = format!(r#"{{"{}":true}}"#, "unknown".repeat(2_000));

    for (tool_name, error) in [
        (
            LIST_WORKFLOWS_TOOL_NAME,
            parse_arguments::<ListWorkflowsArgs>(LIST_WORKFLOWS_TOOL_NAME, &arguments).unwrap_err(),
        ),
        (
            WAIT_WORKFLOWS_TOOL_NAME,
            parse_arguments::<WaitWorkflowsArgs>(WAIT_WORKFLOWS_TOOL_NAME, &arguments).unwrap_err(),
        ),
    ] {
        let FunctionCallError::RespondToModel(message) = error else {
            panic!("{tool_name} should return a model-visible parse error");
        };
        assert!(message.starts_with(&format!("invalid {tool_name} input:")));
        assert!(message.len() <= crate::workflow_result_tool::MODEL_ERROR_MAX_BYTES);
        assert!(message.ends_with("...[truncated]"));
    }
}

fn snapshot(run_id: &str, started_at: i64, status: WorkflowTaskStatus) -> WorkflowTaskSnapshot {
    let root = AbsolutePathBuf::try_from(std::env::temp_dir()).unwrap();
    WorkflowTaskSnapshot {
        thread_id: "thread".to_string(),
        turn_id: "turn".to_string(),
        task_id: format!("task-{run_id}"),
        run_id: run_id.to_string(),
        workflow_name: "status-test".to_string(),
        title: None,
        status,
        summary: "summary".to_string(),
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
        started_at,
        completed_at: workflow_status_is_terminal(status).then_some(started_at + 1),
        script_sha256: "sha256".to_string(),
    }
}

#[derive(Default)]
struct ControlledTurnActivity {
    wake: Notify,
    wait_entered: Notify,
    non_user_wake_processed: Notify,
}

impl ControlledTurnActivity {
    fn signal_non_user_wake(&self) {
        self.wake.notify_one();
    }
}

impl TurnActivitySubscription for ControlledTurnActivity {
    fn observed(&self) -> Option<TurnActivity> {
        None
    }

    fn wait<'a>(&'a self) -> TurnActivityFuture<'a> {
        Box::pin(async move {
            self.wait_entered.notify_one();
            loop {
                self.wake.notified().await;
                self.non_user_wake_processed.notify_one();
            }
        })
    }
}

fn waited_recovery(
    reason: &'static str,
    recovery_eligible: bool,
) -> Option<WorkflowRecoverySummary> {
    recovery_eligible.then(|| {
        let status = match reason {
            "killed" => WorkflowTaskStatus::Killed,
            _ => WorkflowTaskStatus::Failed,
        };
        workflow_recovery_status(&snapshot("wf_recovery-summary", 1, status)).into_summary()
    })
}

struct ActivityEmitter {
    activity: Arc<ControlledTurnActivity>,
}

impl TurnItemEmitter for ActivityEmitter {
    fn emit_started<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn emit_completed<'a>(&'a self, _item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }

    fn turn_activity(&self) -> Option<Arc<dyn TurnActivitySubscription>> {
        Some(self.activity.clone())
    }
}

struct WaitFixture {
    _codex_home: TempDir,
    config: Config,
    thread_id: ThreadId,
    service: WorkflowService,
}

impl WaitFixture {
    async fn new() -> Self {
        let codex_home = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await
            .unwrap();
        let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
        Self {
            _codex_home: codex_home,
            config,
            thread_id,
            service: WorkflowService::new(Arc::new(NoopExtensionEventSink), Weak::new()),
        }
    }

    async fn launch_pending(&self, name: &str) -> String {
        let source = format!(
            "export const meta = {{ name: '{name}', description: 'wait test' }}; return new Promise(() => {{}});"
        );
        let script = validate_workflow_script(&source).unwrap();
        let composition = crate::composition::FrozenWorkflowComposition::empty(&script);
        let launch = self
            .service
            .launch(WorkflowLaunchRequest {
                thread_id: self.thread_id,
                turn_id: format!("turn-{name}"),
                config: self.config.clone(),
                resolved: ResolvedWorkflow {
                    script,
                    args: JsonValue::Null,
                    resume_from_run_id: None,
                    origin: WorkflowOrigin::Inline,
                    shadows_existing: false,
                    composition,
                },
                agent_runner: AgentRunner::new(Weak::<ThreadManager>::new()),
                environments: Vec::new(),
                captured_environments: None,
                environment_location: WorkflowEnvironmentLocation::Local,
                declared_inputs: Default::default(),
            })
            .await
            .unwrap();
        launch.run_id
    }
}
