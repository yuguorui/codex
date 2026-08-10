use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(start_paused = true)]
async fn active_workflow_keeps_owner_loaded_and_replays_pending_completion_before_first_turn()
-> Result<()> {
    const IDLE_UNLOAD_ELAPSE: Duration = Duration::from_secs(30 * 60 + 1);
    let script = r#"export const meta = {
  name: "owner-residency-test",
  description: "Keep the owning thread resident while this run is active",
};
return new Promise(() => {});
"#;
    let mut fixture = start_workflow(script, "Run a resident workflow", "Workflow").await?;
    let started: WorkflowStartedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        fixture.mcp.read_notification("workflow/started"),
    )
    .await??;
    wait_for_turn_completed(&mut fixture.mcp, &fixture.thread_id, &fixture.turn_id).await?;

    let unsubscribe: ThreadUnsubscribeResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::ThreadUnsubscribe {
            request_id,
            params: ThreadUnsubscribeParams {
                thread_id: fixture.thread_id.clone(),
            },
        })
        .await?;
    assert_eq!(unsubscribe.status, ThreadUnsubscribeStatus::Unsubscribed);

    tokio::time::advance(IDLE_UNLOAD_ELAPSE).await;
    tokio::task::yield_now().await;
    let loaded: ThreadLoadedListResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams::default(),
        })
        .await?;
    assert_eq!(loaded.data, vec![fixture.thread_id.clone()]);

    let stopped: WorkflowStopResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: fixture.thread_id.clone(),
                run_id: started.run_id.clone(),
            },
        })
        .await?;
    assert!(stopped.accepted);
    wait_for_listed_workflow_status(
        &mut fixture.mcp,
        &fixture.thread_id,
        &started.run_id,
        WorkflowStatus::Killed,
    )
    .await?;

    let mut unloaded = None;
    for _ in 0..3 {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(IDLE_UNLOAD_ELAPSE).await;
        let loaded: ThreadLoadedListResponse = fixture
            .mcp
            .request(|request_id| ClientRequest::ThreadLoadedList {
                request_id,
                params: ThreadLoadedListParams::default(),
            })
            .await?;
        if !loaded.data.contains(&fixture.thread_id) {
            unloaded = Some(loaded);
            break;
        }
    }
    let unloaded = unloaded.context("owner should unload after workflow finalization")?;
    assert!(!unloaded.data.contains(&fixture.thread_id));

    let rollout_path = find_thread_path_by_id_str(
        fixture._codex_home.path(),
        &fixture.thread_id,
        /*state_db_ctx*/ None,
    )
    .await?
    .context("owning thread rollout should exist")?;
    let rollout = tokio::fs::read_to_string(&rollout_path).await?;
    let filtered_rollout = rollout
        .lines()
        .filter(|line| !line.contains("<workflow_notification>"))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&rollout_path, format!("{filtered_rollout}\n")).await?;
    let delivery_path = fixture
        ._codex_home
        .path()
        .join("sessions")
        .join(&fixture.thread_id)
        .join("workflows")
        .join(format!(".{}.completed-delivery.json", started.run_id));
    let mut delivery: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&delivery_path).await?)?;
    delivery["owningModelAcknowledged"] = false.into();
    tokio::fs::write(&delivery_path, serde_json::to_vec(&delivery)?).await?;

    let resumed: ThreadResumeResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: fixture.thread_id.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(resumed.thread.id, fixture.thread_id);
    let reloaded: ThreadLoadedListResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams::default(),
        })
        .await?;
    assert_eq!(reloaded.data, vec![fixture.thread_id.clone()]);

    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .and(|request: &wiremock::Request| body_contains(request, "Continue"))
        .respond_with(WorkflowNotificationPollingResponder::default())
        .mount(&fixture._server)
        .await;
    let TurnStartResponse { turn } = fixture
        .mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: fixture.thread_id.clone(),
                client_user_message_id: None,
                input: vec![UserInput::Text {
                    text: "Continue".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    wait_for_turn_completed(&mut fixture.mcp, &fixture.thread_id, &turn.id).await?;
    let requests = fixture
        ._server
        .received_requests()
        .await
        .context("failed to read owning thread requests")?;
    let first_post_resume_request = requests
        .iter()
        .find(|request| body_contains(request, "Continue"))
        .context("first post-resume model request should be captured")?;
    assert!(body_contains(
        first_post_resume_request,
        "<workflow_notification>"
    ));
    assert!(body_contains(first_post_resume_request, &started.run_id));
    assert!(body_contains(first_post_resume_request, "<result"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_launch_emits_ordered_notifications_and_is_listed() -> Result<()> {
    let script = r#"export const meta = {
  name: "app-server-test",
  title: "App server test",
  description: "Exercise the public workflow event stream",
  phases: [{ title: "Inspect" }],
};
phase("Inspect");
log("inspection complete");
// Keep the run alive past the parent turn so the completion notice is
// recorded while the owning thread is idle rather than mid-turn.
await new Promise((resolve) => setTimeout(resolve, 2000));
return { ok: true };
"#;
    let mut fixture = start_workflow(script, "Run an immediate workflow", "Workflow").await?;

    let events = collect_workflow_events(&mut fixture.mcp).await?;

    assert_eq!(
        events.methods.first().map(String::as_str),
        Some("workflow/started")
    );
    assert_eq!(
        events.methods.last().map(String::as_str),
        Some("workflow/completed")
    );
    assert!(
        events.methods[1..events.methods.len() - 1]
            .iter()
            .all(|method| method == "workflow/progress")
    );
    assert!(!events.progress.is_empty());
    assert_eq!(events.started.thread_id, fixture.thread_id);
    assert_eq!(events.started.turn_id, fixture.turn_id);
    assert_eq!(events.started.workflow_name, "app-server-test");
    assert_eq!(
        events.started.delivery_key,
        format!(
            "workflow/started/{}/{}/{}",
            events.started.thread_id, events.started.run_id, events.started.task_id
        )
    );
    assert_eq!(events.completed.run_id, events.started.run_id);
    assert_eq!(events.completed.status, WorkflowStatus::Completed);
    assert_eq!(
        events.completed.delivery_key,
        format!(
            "workflow/completed/{}/{}/{}",
            events.completed.thread_id, events.completed.run_id, events.completed.task_id
        )
    );
    assert!(!events.completed.progress_resync_required);

    let list_id = fixture
        .mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: fixture.thread_id.clone(),
            cursor: None,
            limit: None,
        })
        .await?;
    let listed: WorkflowListResponse =
        timeout(DEFAULT_READ_TIMEOUT, fixture.mcp.read_response(list_id)).await??;
    assert_eq!(listed.data.len(), 1);
    assert_eq!(listed.data[0].run_id, events.started.run_id);
    assert_eq!(listed.data[0].status, WorkflowStatus::Completed);

    // The completion notice must reach the owning thread's model context: the
    // next user-triggered turn observes it as a user-role message.
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .and(|request: &wiremock::Request| body_contains(request, "Continue"))
        .respond_with(WorkflowNotificationPollingResponder::default())
        .mount(&fixture._server)
        .await;
    let notified_request: serde_json::Value = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let TurnStartResponse { turn: follow_up } = fixture
                .mcp
                .request(|request_id| ClientRequest::TurnStart {
                    request_id,
                    params: TurnStartParams {
                        thread_id: fixture.thread_id.clone(),
                        client_user_message_id: None,
                        input: vec![UserInput::Text {
                            text: "Continue".to_string(),
                            text_elements: Vec::new(),
                        }],
                        ..Default::default()
                    },
                })
                .await?;
            wait_for_turn_completed(&mut fixture.mcp, &fixture.thread_id, &follow_up.id).await?;
            let requests = fixture
                ._server
                .received_requests()
                .await
                .context("failed to read workflow notification requests")?;
            if let Some(request) = requests.iter().find(|request| {
                body_contains(request, "<workflow_notification>")
                    && body_contains(request, &events.completed.run_id)
            }) {
                return Ok::<_, anyhow::Error>(request.body_json()?);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert!(
        notified_request
            .to_string()
            .contains(&events.completed.run_id),
        "workflow completion notification was not delivered to the owning thread"
    );

    let stop: WorkflowStopResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: fixture.thread_id,
                run_id: events.started.run_id,
            },
        })
        .await?;
    assert!(!stop.accepted);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_stop_accepts_only_an_active_run() -> Result<()> {
    let script = r#"export const meta = {
  name: "pending-test",
  description: "Remain active until stopped",
};
return new Promise(() => {});
"#;
    let mut fixture = start_workflow(script, "Run a pending workflow", "Workflow").await?;
    let started: WorkflowStartedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        fixture.mcp.read_notification("workflow/started"),
    )
    .await??;

    let skip: WorkflowAgentSkipResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowAgentSkip {
            request_id,
            params: WorkflowAgentControlParams {
                thread_id: fixture.thread_id.clone(),
                run_id: started.run_id.clone(),
                agent_index: 0,
            },
        })
        .await?;
    let retry: WorkflowAgentRetryResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowAgentRetry {
            request_id,
            params: WorkflowAgentControlParams {
                thread_id: fixture.thread_id.clone(),
                run_id: started.run_id.clone(),
                agent_index: 0,
            },
        })
        .await?;
    assert!(!skip.accepted);
    assert!(!retry.accepted);

    let first_stop: WorkflowStopResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: fixture.thread_id.clone(),
                run_id: started.run_id.clone(),
            },
        })
        .await?;
    assert!(first_stop.accepted);

    let completed: WorkflowCompletedNotification = timeout(DEFAULT_READ_TIMEOUT, async {
        let mut completed = None;
        let mut turn_completed = false;
        loop {
            let JSONRPCMessage::Notification(notification) =
                fixture.mcp.read_next_message().await?
            else {
                continue;
            };
            let Some(params) = notification.params else {
                continue;
            };
            match notification.method.as_str() {
                "workflow/completed" => {
                    let candidate: WorkflowCompletedNotification = serde_json::from_value(params)?;
                    if candidate.run_id == started.run_id {
                        completed = Some(candidate);
                    }
                }
                "turn/completed" => {
                    let candidate: TurnCompletedNotification = serde_json::from_value(params)?;
                    turn_completed = candidate.thread_id == fixture.thread_id
                        && candidate.turn.id == fixture.turn_id;
                }
                _ => {}
            }
            if turn_completed && completed.is_some() {
                return completed.context("missing workflow/completed notification");
            }
        }
    })
    .await??;
    assert_eq!(completed.run_id, started.run_id);
    assert_eq!(completed.status, WorkflowStatus::Killed);

    let second_stop: WorkflowStopResponse = fixture
        .mcp
        .request(|request_id| ClientRequest::WorkflowStop {
            request_id,
            params: WorkflowStopParams {
                thread_id: fixture.thread_id,
                run_id: started.run_id,
            },
        })
        .await?;
    assert!(!second_stop.accepted);
    Ok(())
}
