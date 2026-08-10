use super::*;
use crate::outgoing_message::OutgoingDelivery;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerNotificationEnvelope;
use codex_app_server_protocol::ThreadRealtimeStartedNotification;
use codex_protocol::protocol::RealtimeConversationVersion;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::timeout;

fn absolute_path(path: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).expect("absolute path")
}

fn thread_realtime_started_notification() -> ServerNotification {
    ServerNotification::ThreadRealtimeStarted(ThreadRealtimeStartedNotification {
        thread_id: "thread-1".to_string(),
        realtime_session_id: None,
        version: RealtimeConversationVersion::V1,
    })
}

fn app_server_notification(notification: ServerNotification) -> OutgoingMessage {
    OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
        notification,
        emitted_at_ms: Some(1_234),
    })
}

#[tokio::test]
async fn tracked_delivery_to_disconnected_connection_is_not_acknowledged() {
    let connection_id = ConnectionId(404);
    let (write_complete_tx, write_complete_rx) = tokio::sync::oneshot::channel();

    route_outgoing_envelope(
        &mut HashMap::new(),
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: app_server_notification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "test".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: Some(write_complete_tx),
        },
    )
    .await;

    assert!(write_complete_rx.await.is_err());
}

#[tokio::test]
async fn tracked_delivery_acknowledges_only_after_writer_completion() {
    let connection_id = ConnectionId(405);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    let mut connections = HashMap::from([(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    )]);
    let (write_complete_tx, mut write_complete_rx) = tokio::sync::oneshot::channel();

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: app_server_notification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "test".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: Some(write_complete_tx),
        },
    )
    .await;

    assert!(matches!(
        write_complete_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    let queued = writer_rx
        .recv()
        .await
        .expect("writer should receive message");
    queued
        .write_complete_tx
        .expect("tracked write should include completion sender")
        .send(OutgoingWriteResult::Written)
        .expect("delivery waiter should remain online");
    assert_eq!(
        write_complete_rx.await.expect("writer should acknowledge"),
        OutgoingWriteResult::Written
    );
}

#[tokio::test]
async fn tracked_opt_out_is_not_a_target() {
    let connection_id = ConnectionId(406);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    let mut connections = HashMap::from([(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::from(["configWarning".to_string()]))),
            /*disconnect_sender*/ None,
        ),
    )]);
    let (write_complete_tx, write_complete_rx) = tokio::sync::oneshot::channel();

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: app_server_notification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "test".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: Some(write_complete_tx),
        },
    )
    .await;

    assert_eq!(
        write_complete_rx
            .await
            .expect("filter should settle delivery"),
        OutgoingWriteResult::NotTarget
    );
    assert!(writer_rx.try_recv().is_err());
}

#[tokio::test]
async fn tracked_experimental_filter_is_not_a_target() {
    let connection_id = ConnectionId(407);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    let mut connections = HashMap::from([(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    )]);
    let (write_complete_tx, write_complete_rx) = tokio::sync::oneshot::channel();

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: app_server_notification(thread_realtime_started_notification()),
            write_complete_tx: Some(write_complete_tx),
        },
    )
    .await;

    assert_eq!(
        write_complete_rx
            .await
            .expect("filter should settle delivery"),
        OutgoingWriteResult::NotTarget
    );
    assert!(writer_rx.try_recv().is_err());
}

#[tokio::test]
async fn tracked_fanout_reports_partial_writer_success_per_subscriber() {
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(2);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    let written_connection = ConnectionId(408);
    let disconnected_connection = ConnectionId(409);
    let (written_tx, mut written_rx) = mpsc::channel(1);
    let (disconnected_tx, disconnected_rx) = mpsc::channel(1);
    drop(disconnected_rx);
    let initialized = Arc::new(AtomicBool::new(true));
    let experimental_api_enabled = Arc::new(AtomicBool::new(true));
    let opted_out = Arc::new(RwLock::new(HashSet::new()));
    let mut connections = HashMap::from([
        (
            written_connection,
            OutboundConnectionState::new(
                written_tx,
                Arc::clone(&initialized),
                Arc::clone(&experimental_api_enabled),
                Arc::clone(&opted_out),
                /*disconnect_sender*/ None,
            ),
        ),
        (
            disconnected_connection,
            OutboundConnectionState::new(
                disconnected_tx,
                initialized,
                experimental_api_enabled,
                opted_out,
                /*disconnect_sender*/ None,
            ),
        ),
    ]);
    let delivery = tokio::spawn({
        let outgoing = Arc::clone(&outgoing);
        async move {
            outgoing
                .try_send_server_notification_to_connections_with_timeout(
                    &[written_connection, disconnected_connection],
                    ServerNotification::ConfigWarning(ConfigWarningNotification {
                        summary: "test".to_string(),
                        details: None,
                        path: None,
                        range: None,
                    }),
                    Duration::from_secs(1),
                )
                .await
        }
    });

    for _ in 0..2 {
        let envelope = outgoing_rx
            .recv()
            .await
            .expect("fanout should enqueue message");
        route_outgoing_envelope(&mut connections, envelope).await;
    }
    written_rx
        .recv()
        .await
        .expect("connected writer should receive message")
        .write_complete_tx
        .expect("tracked message should carry writer acknowledgment")
        .send(OutgoingWriteResult::Written)
        .expect("fanout should still await writer");

    let delivery = delivery.await.expect("fanout task should finish");
    assert!(!delivery.truncated);
    assert_eq!(delivery.subscribers.len(), 2);
    assert!(delivery.subscribers.iter().any(|subscriber| {
        subscriber.connection_id == written_connection
            && subscriber.outcome == OutgoingDelivery::Written
    }));
    assert!(delivery.subscribers.iter().any(|subscriber| {
        subscriber.connection_id == disconnected_connection
            && subscriber.outcome == OutgoingDelivery::Retryable
    }));
}

#[tokio::test]
async fn tracked_fanout_times_out_after_writer_accepts_without_completing() {
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(1);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    let connection_id = ConnectionId(410);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    let mut connections = HashMap::from([(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    )]);
    let delivery = tokio::spawn({
        let outgoing = Arc::clone(&outgoing);
        async move {
            outgoing
                .try_send_server_notification_to_connections_with_timeout(
                    &[connection_id],
                    ServerNotification::ConfigWarning(ConfigWarningNotification {
                        summary: "test".to_string(),
                        details: None,
                        path: None,
                        range: None,
                    }),
                    Duration::from_millis(10),
                )
                .await
        }
    });
    let envelope = outgoing_rx
        .recv()
        .await
        .expect("fanout should enqueue message");
    route_outgoing_envelope(&mut connections, envelope).await;
    let queued = writer_rx
        .recv()
        .await
        .expect("writer should accept tracked message");

    let delivery = timeout(Duration::from_secs(1), delivery)
        .await
        .expect("write deadline should finish")
        .expect("fanout task should not panic");
    assert_eq!(delivery.subscribers.len(), 1);
    assert_eq!(delivery.subscribers[0].outcome, OutgoingDelivery::Retryable);
    assert!(
        queued
            .write_complete_tx
            .expect("tracked message should carry writer acknowledgment")
            .send(OutgoingWriteResult::Written)
            .is_err()
    );
}

#[tokio::test]
async fn to_connection_notification_respects_opt_out_filters() {
    let connection_id = ConnectionId(7);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    let initialized = Arc::new(AtomicBool::new(true));
    let opted_out_notification_methods =
        Arc::new(RwLock::new(HashSet::from(["configWarning".to_string()])));

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            initialized,
            Arc::new(AtomicBool::new(true)),
            opted_out_notification_methods,
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: app_server_notification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "task_started".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: None,
        },
    )
    .await;

    assert!(
        writer_rx.try_recv().is_err(),
        "opted-out notification should be dropped"
    );
}

#[tokio::test]
async fn to_connection_notifications_are_dropped_for_opted_out_clients() {
    let connection_id = ConnectionId(10);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::from(["configWarning".to_string()]))),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: app_server_notification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "task_started".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: None,
        },
    )
    .await;

    assert!(
        writer_rx.try_recv().is_err(),
        "opted-out notifications should not reach clients"
    );
}

#[tokio::test]
async fn to_connection_notifications_are_preserved_for_non_opted_out_clients() {
    let connection_id = ConnectionId(11);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: app_server_notification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "task_started".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("notification should reach non-opted-out clients");
    assert!(matches!(
        message.message,
        OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
            notification: ServerNotification::ConfigWarning(ConfigWarningNotification { summary, .. }),
            ..
        }) if summary == "task_started"
    ));
}

#[tokio::test]
async fn experimental_notifications_are_dropped_without_capability() {
    let connection_id = ConnectionId(12);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: app_server_notification(thread_realtime_started_notification()),
            write_complete_tx: None,
        },
    )
    .await;

    assert!(
        writer_rx.try_recv().is_err(),
        "experimental notifications should not reach clients without capability"
    );
}

#[tokio::test]
async fn experimental_notifications_are_preserved_with_capability() {
    let connection_id = ConnectionId(13);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: app_server_notification(thread_realtime_started_notification()),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("experimental notification should reach opted-in client");
    assert!(matches!(
        message.message,
        OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
            notification: ServerNotification::ThreadRealtimeStarted(_),
            ..
        })
    ));
}

#[tokio::test]
async fn command_execution_request_approval_strips_additional_permissions_without_capability() {
    let connection_id = ConnectionId(8);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
                request_id: RequestId::Integer(1),
                params: codex_app_server_protocol::CommandExecutionRequestApprovalParams {
                    thread_id: "thr_123".to_string(),
                    turn_id: "turn_123".to_string(),
                    item_id: "call_123".to_string(),
                    started_at_ms: 0,
                    approval_id: None,
                    environment_id: None,
                    reason: Some("Need extra read access".to_string()),
                    network_approval_context: None,
                    command: Some("cat file".to_string()),
                    cwd: Some(absolute_path("/tmp").into()),
                    command_actions: None,
                    additional_permissions: Some(
                        codex_app_server_protocol::AdditionalPermissionProfile {
                            network: None,
                            file_system: Some(
                                codex_app_server_protocol::AdditionalFileSystemPermissions {
                                    read: Some(vec![absolute_path("/tmp/allowed").into()]),
                                    write: None,
                                    glob_scan_max_depth: None,
                                    entries: None,
                                },
                            ),
                        },
                    ),
                    proposed_execpolicy_amendment: None,
                    proposed_network_policy_amendments: None,
                    available_decisions: None,
                },
            }),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("request should be delivered to the connection");
    let json = serde_json::to_value(message.message).expect("request should serialize");
    assert_eq!(json["params"].get("additionalPermissions"), None);
}

#[tokio::test]
async fn command_execution_request_approval_keeps_additional_permissions_with_capability() {
    let connection_id = ConnectionId(9);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
                request_id: RequestId::Integer(1),
                params: codex_app_server_protocol::CommandExecutionRequestApprovalParams {
                    thread_id: "thr_123".to_string(),
                    turn_id: "turn_123".to_string(),
                    item_id: "call_123".to_string(),
                    started_at_ms: 0,
                    approval_id: None,
                    environment_id: None,
                    reason: Some("Need extra read access".to_string()),
                    network_approval_context: None,
                    command: Some("cat file".to_string()),
                    cwd: Some(absolute_path("/tmp").into()),
                    command_actions: None,
                    additional_permissions: Some(
                        codex_app_server_protocol::AdditionalPermissionProfile {
                            network: None,
                            file_system: Some(
                                codex_app_server_protocol::AdditionalFileSystemPermissions {
                                    read: Some(vec![absolute_path("/tmp/allowed").into()]),
                                    write: None,
                                    glob_scan_max_depth: None,
                                    entries: None,
                                },
                            ),
                        },
                    ),
                    proposed_execpolicy_amendment: None,
                    proposed_network_policy_amendments: None,
                    available_decisions: None,
                },
            }),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("request should be delivered to the connection");
    let json = serde_json::to_value(message.message).expect("request should serialize");
    let allowed_path = absolute_path("/tmp/allowed").to_string_lossy().into_owned();
    assert_eq!(
        json["params"]["additionalPermissions"],
        json!({
            "network": null,
            "fileSystem": {
                "read": [allowed_path],
            "write": null,
            },
        })
    );
}

#[tokio::test]
async fn broadcast_does_not_block_on_slow_connection() {
    let fast_connection_id = ConnectionId(1);
    let slow_connection_id = ConnectionId(2);

    let (fast_writer_tx, mut fast_writer_rx) = mpsc::channel(1);
    let (slow_writer_tx, mut slow_writer_rx) = mpsc::channel(1);
    let fast_disconnect_token = CancellationToken::new();
    let slow_disconnect_token = CancellationToken::new();

    let mut connections = HashMap::new();
    connections.insert(
        fast_connection_id,
        OutboundConnectionState::new(
            fast_writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            Some(fast_disconnect_token.clone()),
        ),
    );
    connections.insert(
        slow_connection_id,
        OutboundConnectionState::new(
            slow_writer_tx.clone(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            Some(slow_disconnect_token.clone()),
        ),
    );

    let queued_message = app_server_notification(ServerNotification::ConfigWarning(
        ConfigWarningNotification {
            summary: "already-buffered".to_string(),
            details: None,
            path: None,
            range: None,
        },
    ));
    slow_writer_tx
        .try_send(QueuedOutgoingMessage::new(queued_message))
        .expect("channel should have room");

    let broadcast_message = app_server_notification(ServerNotification::ConfigWarning(
        ConfigWarningNotification {
            summary: "test".to_string(),
            details: None,
            path: None,
            range: None,
        },
    ));
    timeout(
        Duration::from_millis(100),
        route_outgoing_envelope(
            &mut connections,
            OutgoingEnvelope::Broadcast {
                message: broadcast_message,
            },
        ),
    )
    .await
    .expect("broadcast should return even when one connection is slow");
    assert!(!connections.contains_key(&slow_connection_id));
    assert!(slow_disconnect_token.is_cancelled());
    assert!(!fast_disconnect_token.is_cancelled());
    let fast_message = fast_writer_rx
        .try_recv()
        .expect("fast connection should receive the broadcast notification");
    assert!(matches!(
        fast_message.message,
        OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
            notification: ServerNotification::ConfigWarning(ConfigWarningNotification { summary, .. }),
            ..
        }) if summary == "test"
    ));

    let slow_message = slow_writer_rx
        .try_recv()
        .expect("slow connection should retain its original buffered message");
    assert!(matches!(
        slow_message.message,
        OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
            notification: ServerNotification::ConfigWarning(ConfigWarningNotification { summary, .. }),
            ..
        }) if summary == "already-buffered"
    ));
}

#[tokio::test]
async fn to_connection_stdio_waits_instead_of_disconnecting_when_writer_queue_is_full() {
    let connection_id = ConnectionId(3);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    writer_tx
        .send(QueuedOutgoingMessage::new(app_server_notification(
            ServerNotification::ConfigWarning(ConfigWarningNotification {
                summary: "queued".to_string(),
                details: None,
                path: None,
                range: None,
            }),
        )))
        .await
        .expect("channel should accept the first queued message");

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    let route_task = tokio::spawn(async move {
        route_outgoing_envelope(
            &mut connections,
            OutgoingEnvelope::ToConnection {
                connection_id,
                message: app_server_notification(ServerNotification::ConfigWarning(
                    ConfigWarningNotification {
                        summary: "second".to_string(),
                        details: None,
                        path: None,
                        range: None,
                    },
                )),
                write_complete_tx: None,
            },
        )
        .await
    });

    let first = timeout(Duration::from_millis(100), writer_rx.recv())
        .await
        .expect("first queued message should be readable")
        .expect("first queued message should exist");
    timeout(Duration::from_millis(100), route_task)
        .await
        .expect("routing should finish after the first queued message is drained")
        .expect("routing task should succeed");

    assert!(matches!(
        first.message,
        OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
            notification: ServerNotification::ConfigWarning(ConfigWarningNotification { summary, .. }),
            ..
        }) if summary == "queued"
    ));
    let second = writer_rx
        .try_recv()
        .expect("second notification should be delivered once the queue has room");
    assert!(matches!(
        second.message,
        OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
            notification: ServerNotification::ConfigWarning(ConfigWarningNotification { summary, .. }),
            ..
        }) if summary == "second"
    ));
}
