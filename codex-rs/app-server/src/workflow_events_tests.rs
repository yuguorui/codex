use super::*;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingWriteResult;
use crate::thread_state::ConnectionCapabilities;
use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::ServerNotification;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;

#[derive(Debug, PartialEq, Eq)]
enum ObservedNotification {
    Started(String, String, String),
    Progress(String, String, u64),
    Completed(String, String, String, bool),
}

#[test]
fn completed_notification_preserves_agent_outcome_counts() {
    let thread_id = codex_protocol::ThreadId::from_string("11111111-1111-4111-8111-111111111111")
        .expect("valid thread id");
    let mut event = completed_event(thread_id, "run-a", "task-a");
    event.usage = core::WorkflowUsage {
        successful_agent_count: 2,
        failed_agent_count: 1,
        skipped_agent_count: 1,
        null_agent_result_count: 1,
        ..Default::default()
    };

    let notification = completed(event, "delivery-key".to_string(), false).usage;

    assert_eq!(
        notification,
        WorkflowUsage {
            successful_agent_count: 2,
            failed_agent_count: 1,
            skipped_agent_count: 1,
            null_agent_result_count: 1,
            ..Default::default()
        }
    );
}

#[tokio::test]
async fn lifecycle_acknowledges_only_after_transport_write() {
    let (sender, mut outgoing_rx, thread_id) = notification_fixture().await;
    let delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(thread_id, "run-a", "task-a"))
                .await
        }
    });

    let (observed, write_complete) = next_notification(&mut outgoing_rx).await;
    assert_eq!(
        observed,
        ObservedNotification::Started(
            "run-a".to_string(),
            "task-a".to_string(),
            format!("workflow/started/{thread_id}/run-a/task-a"),
        )
    );
    assert!(!delivery.is_finished());
    write_complete
        .send(OutgoingWriteResult::Written)
        .expect("delivery should still be waiting for transport write");
    assert_eq!(
        delivery.await.expect("delivery task should finish"),
        ExtensionEventDelivery::Acknowledged {
            idempotency_key: format!("workflow/started/{thread_id}/run-a/task-a"),
        }
    );
}

#[tokio::test]
async fn disconnect_before_write_keeps_lifecycle_retryable() {
    let (sender, mut outgoing_rx, thread_id) = notification_fixture().await;
    let delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .completed(completed_event(thread_id, "run-a", "task-a"))
                .await
        }
    });

    let (_, write_complete) = next_notification(&mut outgoing_rx).await;
    drop(write_complete);
    assert_eq!(
        delivery.await.expect("delivery task should finish"),
        ExtensionEventDelivery::Retryable {
            idempotency_key: format!("workflow/completed/{thread_id}/run-a/task-a"),
        }
    );
}

#[tokio::test]
async fn write_ack_timeout_keeps_started_retryable() {
    let (sender, mut outgoing_rx, thread_id) =
        notification_fixture_with_timeout(Duration::from_millis(10)).await;
    let delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(thread_id, "run-timeout", "task-timeout"))
                .await
        }
    });

    let (_, write_complete) = next_notification(&mut outgoing_rx).await;
    assert_eq!(
        timeout(Duration::from_secs(1), delivery)
            .await
            .expect("write acknowledgment deadline should complete")
            .expect("delivery task should finish"),
        ExtensionEventDelivery::Retryable {
            idempotency_key: format!("workflow/started/{thread_id}/run-timeout/task-timeout"),
        }
    );
    assert!(write_complete.send(OutgoingWriteResult::Written).is_err());
}

#[tokio::test]
async fn partial_success_retries_only_failed_subscriber_and_duplicate_key_is_stable() {
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(8);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        AnalyticsEventsClient::disabled(),
    ));
    let thread_id = codex_protocol::ThreadId::new();
    let thread_state_manager = ThreadStateManager::new();
    for connection_id in [ConnectionId(1), ConnectionId(2)] {
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
            )
            .await
            .expect("connection should be subscribed");
    }
    let sender = WorkflowNotificationSender::new(outgoing, thread_state_manager);
    let expected_key = format!("workflow/started/{thread_id}/run-a/task-a");

    let first_attempt = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(thread_id, "run-a", "task-a"))
                .await
        }
    });
    let (first_connection, first_event, first_write) =
        next_notification_with_connection(&mut outgoing_rx).await;
    let (second_connection, second_event, second_write) =
        next_notification_with_connection(&mut outgoing_rx).await;
    assert_ne!(first_connection, second_connection);
    assert_eq!(first_event, second_event);
    first_write
        .send(OutgoingWriteResult::Written)
        .expect("first subscriber should acknowledge");
    drop(second_write);
    assert!(matches!(
        first_attempt.await.expect("first attempt should finish"),
        ExtensionEventDelivery::Retryable { .. }
    ));

    let retry = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(thread_id, "run-a", "task-a"))
                .await
        }
    });
    let (retried_connection, retried_event, retried_write) =
        next_notification_with_connection(&mut outgoing_rx).await;
    assert_eq!(retried_connection, second_connection);
    assert_eq!(retried_event, second_event);
    retried_write
        .send(OutgoingWriteResult::Written)
        .expect("failed subscriber should acknowledge retry");
    assert_eq!(
        retry.await.expect("retry should finish"),
        ExtensionEventDelivery::Acknowledged {
            idempotency_key: expected_key.clone(),
        }
    );

    assert_eq!(
        sender
            .started(started_event(thread_id, "run-a", "task-a"))
            .await,
        ExtensionEventDelivery::Acknowledged {
            idempotency_key: expected_key,
        }
    );
    assert!(outgoing_rx.try_recv().is_err());
}

#[tokio::test]
async fn lifecycle_retry_discovers_replacement_for_disconnected_subscriber() {
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(8);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        AnalyticsEventsClient::disabled(),
    ));
    let thread_id = codex_protocol::ThreadId::new();
    let thread_state_manager = ThreadStateManager::new();
    let sender = WorkflowNotificationSender::new(outgoing, thread_state_manager.clone());
    let disconnected_connection = ConnectionId(7);
    thread_state_manager
        .connection_initialized(disconnected_connection, ConnectionCapabilities::default())
        .await;
    thread_state_manager
        .try_ensure_connection_subscribed(
            thread_id,
            disconnected_connection,
            /*experimental_raw_events*/ false,
        )
        .await
        .expect("connection should be subscribed");

    let first_attempt = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(thread_id, "run-online", "task-online"))
                .await
        }
    });
    let (observed_connection, _, write_complete) =
        next_notification_with_connection(&mut outgoing_rx).await;
    assert_eq!(observed_connection, disconnected_connection);
    drop(write_complete);
    assert!(matches!(
        first_attempt.await.expect("first attempt should finish"),
        ExtensionEventDelivery::Retryable { .. }
    ));
    let _ = thread_state_manager
        .remove_connection(disconnected_connection)
        .await;

    let online_connection = ConnectionId(8);
    thread_state_manager
        .connection_initialized(online_connection, ConnectionCapabilities::default())
        .await;
    thread_state_manager
        .try_ensure_connection_subscribed(
            thread_id,
            online_connection,
            /*experimental_raw_events*/ false,
        )
        .await
        .expect("replacement connection should be subscribed");

    let retry = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(thread_id, "run-online", "task-online"))
                .await
        }
    });
    let (observed_connection, _, write_complete) =
        next_notification_with_connection(&mut outgoing_rx).await;
    assert_eq!(observed_connection, online_connection);
    write_complete
        .send(OutgoingWriteResult::Written)
        .expect("online subscriber should acknowledge");
    assert!(matches!(
        retry.await.expect("retry should finish"),
        ExtensionEventDelivery::Acknowledged { .. }
    ));
}

#[tokio::test]
async fn lifecycle_emit_delivers_more_than_128_subscribers_across_bounded_pages() {
    const SUBSCRIBER_COUNT: usize = 129;
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(TRACKED_FANOUT_CAPACITY + 1);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        AnalyticsEventsClient::disabled(),
    ));
    let thread_id = codex_protocol::ThreadId::new();
    let thread_state_manager = ThreadStateManager::new();
    for index in 0..SUBSCRIBER_COUNT {
        let connection_id = ConnectionId(u64::try_from(index + 1).expect("index should fit"));
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
            )
            .await
            .expect("connection should be subscribed");
    }
    let sender = WorkflowNotificationSender::new(outgoing, thread_state_manager);

    let delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(thread_id, "run-page", "task-page"))
                .await
        }
    });
    for expected in 1..=SUBSCRIBER_COUNT {
        let (connection_id, _, write_complete) =
            next_notification_with_connection(&mut outgoing_rx).await;
        assert_eq!(
            connection_id,
            ConnectionId(u64::try_from(expected).expect("index should fit"))
        );
        write_complete
            .send(OutgoingWriteResult::Written)
            .expect("subscriber should acknowledge");
    }
    assert!(matches!(
        delivery.await.expect("fanout should finish"),
        ExtensionEventDelivery::Acknowledged { .. }
    ));
    assert!(outgoing_rx.try_recv().is_err());
}

#[tokio::test]
async fn same_execution_preserves_started_progress_completed_order() {
    let (sender, mut outgoing_rx, thread_id) = notification_fixture().await;
    let started_delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(thread_id, "run-a", "task-a"))
                .await
        }
    });
    let (started, started_write) = next_notification(&mut outgoing_rx).await;
    started_write
        .send(OutgoingWriteResult::Written)
        .expect("started write should complete");
    started_delivery
        .await
        .expect("started delivery task should finish");

    sender.progress(progress_event(thread_id, "run-a", "task-a", 42));
    let completed_delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .completed(completed_event(thread_id, "run-a", "task-a"))
                .await
        }
    });
    let (progress, progress_write) = next_notification(&mut outgoing_rx).await;
    progress_write
        .send(OutgoingWriteResult::Written)
        .expect("progress write should complete");
    let (completed, completed_write) = next_notification(&mut outgoing_rx).await;
    completed_write
        .send(OutgoingWriteResult::Written)
        .expect("completed write should complete");
    completed_delivery
        .await
        .expect("completed delivery task should finish");

    assert_eq!(
        vec![started, progress, completed],
        vec![
            ObservedNotification::Started(
                "run-a".to_string(),
                "task-a".to_string(),
                format!("workflow/started/{thread_id}/run-a/task-a"),
            ),
            ObservedNotification::Progress("run-a".to_string(), "task-a".to_string(), 42),
            ObservedNotification::Completed(
                "run-a".to_string(),
                "task-a".to_string(),
                format!("workflow/completed/{thread_id}/run-a/task-a"),
                false,
            ),
        ]
    );
}

#[tokio::test]
async fn successful_later_progress_clears_resync_requirement() {
    let (sender, mut outgoing_rx, thread_id) = notification_fixture().await;
    let started_delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(thread_id, "run-a", "task-a"))
                .await
        }
    });
    let (_, started_write) = next_notification(&mut outgoing_rx).await;
    started_write
        .send(OutgoingWriteResult::Written)
        .expect("started write should complete");
    started_delivery
        .await
        .expect("started delivery should finish");

    sender.progress(progress_event(thread_id, "run-a", "task-a", 1));
    let (_, failed_progress_write) = next_notification(&mut outgoing_rx).await;
    drop(failed_progress_write);
    wait_for_resync_state(&sender, thread_id, true).await;

    sender.progress(progress_event(thread_id, "run-a", "task-a", 2));
    let (_, recovered_progress_write) = next_notification(&mut outgoing_rx).await;
    recovered_progress_write
        .send(OutgoingWriteResult::Written)
        .expect("later progress should write");
    wait_for_resync_state(&sender, thread_id, false).await;

    let completed_delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .completed(completed_event(thread_id, "run-a", "task-a"))
                .await
        }
    });
    let (completed, completed_write) = next_notification(&mut outgoing_rx).await;
    assert!(matches!(
        completed,
        ObservedNotification::Completed(_, _, _, false)
    ));
    completed_write
        .send(OutgoingWriteResult::Written)
        .expect("completed write should finish");
    completed_delivery
        .await
        .expect("completed delivery should finish");
}

#[tokio::test]
async fn blocked_thread_does_not_block_another_thread() {
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(8);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        AnalyticsEventsClient::disabled(),
    ));
    let thread_state_manager = ThreadStateManager::new();
    let first_thread = codex_protocol::ThreadId::new();
    let second_thread = codex_protocol::ThreadId::new();
    for (thread_id, connection_id) in [
        (first_thread, ConnectionId(1)),
        (second_thread, ConnectionId(2)),
    ] {
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
            )
            .await
            .expect("connection should be subscribed");
    }
    let sender = WorkflowNotificationSender::new(outgoing, thread_state_manager);

    let first_delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(first_thread, "run-a", "task-a"))
                .await
        }
    });
    let (_, first_write) = next_notification(&mut outgoing_rx).await;

    let second_delivery = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .started(started_event(second_thread, "run-b", "task-b"))
                .await
        }
    });
    let (second_observed, second_write) = next_notification(&mut outgoing_rx).await;
    assert_eq!(
        second_observed,
        ObservedNotification::Started(
            "run-b".to_string(),
            "task-b".to_string(),
            format!("workflow/started/{second_thread}/run-b/task-b"),
        )
    );
    second_write
        .send(OutgoingWriteResult::Written)
        .expect("second write should be observed");
    timeout(Duration::from_secs(1), second_delivery)
        .await
        .expect("second thread should not wait for the first")
        .expect("second delivery task should finish");
    assert!(!first_delivery.is_finished());
    first_write
        .send(OutgoingWriteResult::Written)
        .expect("first write should be observed");
    first_delivery
        .await
        .expect("first delivery task should finish");
}

#[test]
fn lifecycle_state_has_a_hard_global_bound() {
    let thread_id = codex_protocol::ThreadId::new();
    let mut state = WorkflowNotificationState::default();
    let mut receivers = Vec::new();
    for index in 0..WORKFLOW_LIFECYCLE_BUFFER_CAPACITY {
        let (delivered, delivered_rx) = oneshot::channel();
        receivers.push(delivered_rx);
        assert!(
            state
                .enqueue_lifecycle(PendingLifecycleNotification {
                    notification: LifecycleNotification::Completed(completed_event(
                        thread_id,
                        &format!("run-{index}"),
                        &format!("task-{index}"),
                    )),
                    delivered,
                })
                .is_ok()
        );
    }

    let (overflow_delivery, _overflow_rx) = oneshot::channel();
    assert!(
        state
            .enqueue_lifecycle(PendingLifecycleNotification {
                notification: LifecycleNotification::Completed(completed_event(
                    thread_id,
                    "overflow",
                    "overflow-task",
                )),
                delivered: overflow_delivery,
            })
            .is_err()
    );
    assert_eq!(
        (state.lifecycle_count, state.lanes.len()),
        (
            WORKFLOW_LIFECYCLE_BUFFER_CAPACITY,
            WORKFLOW_LIFECYCLE_BUFFER_CAPACITY
        )
    );
}

#[test]
fn evicted_progress_marks_completion_for_resync() {
    let thread_id = codex_protocol::ThreadId::new();
    let mut state = WorkflowNotificationState::default();
    for index in 0..=WORKFLOW_EXECUTION_BUFFER_CAPACITY {
        state.insert(progress_event(
            thread_id,
            &format!("run-{index}"),
            &format!("task-{index}"),
            u64::try_from(index).expect("index should fit in u64"),
        ));
    }
    let evicted_key = WorkflowExecutionKey {
        thread_id,
        run_id: "run-0".to_string(),
        task_id: "task-0".to_string(),
    };
    assert!(!state.lanes.contains_key(&evicted_key));

    let (delivered, _delivered_rx) = oneshot::channel();
    assert!(
        state
            .enqueue_lifecycle(PendingLifecycleNotification {
                notification: LifecycleNotification::Completed(completed_event(
                    thread_id, "run-0", "task-0",
                )),
                delivered,
            })
            .is_ok()
    );
    let Some(WorkflowDeliveryAction::Lifecycle {
        progress_resync_required,
        ..
    }) = state.next_action(&evicted_key)
    else {
        panic!("expected completion delivery action");
    };
    assert!(progress_resync_required);
    assert!(
        completed(
            completed_event(thread_id, "run-0", "task-0"),
            "delivery-key".to_string(),
            true,
        )
        .progress_resync_required
    );
}

async fn notification_fixture() -> (
    WorkflowNotificationSender,
    mpsc::Receiver<OutgoingEnvelope>,
    codex_protocol::ThreadId,
) {
    notification_fixture_with_timeout(TRACKED_WRITE_ACK_TIMEOUT).await
}

async fn notification_fixture_with_timeout(
    write_ack_timeout: Duration,
) -> (
    WorkflowNotificationSender,
    mpsc::Receiver<OutgoingEnvelope>,
    codex_protocol::ThreadId,
) {
    let (outgoing_tx, outgoing_rx) = mpsc::channel(8);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        AnalyticsEventsClient::disabled(),
    ));
    let thread_id = codex_protocol::ThreadId::new();
    let connection_id = ConnectionId(1);
    let thread_state_manager = ThreadStateManager::new();
    thread_state_manager
        .connection_initialized(connection_id, ConnectionCapabilities::default())
        .await;
    thread_state_manager
        .try_ensure_connection_subscribed(
            thread_id,
            connection_id,
            /*experimental_raw_events*/ false,
        )
        .await
        .expect("connection should be subscribed");
    (
        WorkflowNotificationSender::new_with_write_ack_timeout(
            outgoing,
            thread_state_manager,
            write_ack_timeout,
        ),
        outgoing_rx,
        thread_id,
    )
}

async fn next_notification(
    outgoing_rx: &mut mpsc::Receiver<OutgoingEnvelope>,
) -> (ObservedNotification, oneshot::Sender<OutgoingWriteResult>) {
    let (_, observed, write_complete) = next_notification_with_connection(outgoing_rx).await;
    (observed, write_complete)
}

async fn next_notification_with_connection(
    outgoing_rx: &mut mpsc::Receiver<OutgoingEnvelope>,
) -> (
    ConnectionId,
    ObservedNotification,
    oneshot::Sender<OutgoingWriteResult>,
) {
    let envelope = timeout(Duration::from_secs(2), outgoing_rx.recv())
        .await
        .expect("timed out waiting for workflow notification")
        .expect("outgoing channel closed unexpectedly");
    let OutgoingEnvelope::ToConnection {
        connection_id,
        message,
        write_complete_tx,
    } = envelope
    else {
        panic!("expected connection-targeted workflow notification");
    };
    let OutgoingMessage::AppServerNotification(envelope) = message else {
        panic!("expected app-server workflow notification");
    };
    let observed = match envelope.notification {
        ServerNotification::WorkflowStarted(event) => {
            ObservedNotification::Started(event.run_id, event.task_id, event.delivery_key)
        }
        ServerNotification::WorkflowProgress(event) => {
            ObservedNotification::Progress(event.run_id, event.task_id, event.usage.total_tokens)
        }
        ServerNotification::WorkflowCompleted(event) => ObservedNotification::Completed(
            event.run_id,
            event.task_id,
            event.delivery_key,
            event.progress_resync_required,
        ),
        notification => panic!("expected workflow notification, got {notification:?}"),
    };
    (
        connection_id,
        observed,
        write_complete_tx.expect("workflow delivery should track transport write"),
    )
}

async fn wait_for_resync_state(
    sender: &WorkflowNotificationSender,
    thread_id: codex_protocol::ThreadId,
    expected: bool,
) {
    timeout(Duration::from_secs(1), async {
        loop {
            let observed = sender
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .lanes
                .iter()
                .find_map(|(key, lane)| {
                    (key.thread_id == thread_id).then_some(lane.progress_resync_required)
                });
            if observed == Some(expected) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("resync state should converge");
}

fn started_event(
    thread_id: codex_protocol::ThreadId,
    run_id: &str,
    task_id: &str,
) -> core::WorkflowStartedEvent {
    let path = temporary_absolute_path();
    core::WorkflowStartedEvent {
        thread_id,
        turn_id: "turn-1".to_string(),
        task_id: task_id.to_string(),
        run_id: run_id.to_string(),
        workflow_name: "test-workflow".to_string(),
        title: None,
        summary: "running".to_string(),
        transcript_dir: path.clone(),
        script_path: path,
        started_at: 1,
    }
}

fn progress_event(
    thread_id: codex_protocol::ThreadId,
    run_id: &str,
    task_id: &str,
    total_tokens: u64,
) -> core::WorkflowProgressEvent {
    core::WorkflowProgressEvent {
        thread_id,
        turn_id: "turn-1".to_string(),
        task_id: task_id.to_string(),
        run_id: run_id.to_string(),
        progress: Vec::new(),
        usage: core::WorkflowUsage {
            total_tokens,
            ..Default::default()
        },
    }
}

fn completed_event(
    thread_id: codex_protocol::ThreadId,
    run_id: &str,
    task_id: &str,
) -> core::WorkflowCompletedEvent {
    core::WorkflowCompletedEvent {
        thread_id,
        turn_id: "turn-1".to_string(),
        task_id: task_id.to_string(),
        run_id: run_id.to_string(),
        workflow_name: "test-workflow".to_string(),
        status: core::WorkflowTaskStatus::Completed,
        summary: "completed".to_string(),
        output_file: temporary_absolute_path(),
        error: None,
        failures: Vec::new(),
        usage: core::WorkflowUsage::default(),
        completed_at: 2,
    }
}

fn temporary_absolute_path() -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(std::env::temp_dir()).expect("temp dir should be absolute")
}
