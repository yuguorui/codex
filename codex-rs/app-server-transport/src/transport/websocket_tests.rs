use super::*;
use crate::OutgoingMessage;
use crate::OutgoingWriteResult;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerNotificationEnvelope;
use futures::sink;
use tokio::time::Duration;
use tokio::time::timeout;

fn notification() -> OutgoingMessage {
    OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
        notification: ServerNotification::ConfigWarning(ConfigWarningNotification {
            summary: "test".to_string(),
            details: None,
            path: None,
            range: None,
        }),
        emitted_at_ms: Some(1),
    })
}

#[tokio::test]
async fn writer_acknowledges_after_sink_accepts_message() {
    let (writer_tx, writer_rx) = mpsc::channel(1);
    let (writer_control_tx, writer_control_rx) = mpsc::channel(1);
    let disconnect_token = CancellationToken::new();
    let writer = tokio::spawn(run_websocket_outbound_loop(
        sink::drain::<AxumWebSocketMessage>(),
        writer_rx,
        writer_control_rx,
        disconnect_token,
    ));
    let (write_complete_tx, write_complete_rx) = tokio::sync::oneshot::channel();

    writer_tx
        .send(QueuedOutgoingMessage {
            message: notification(),
            write_complete_tx: Some(write_complete_tx),
        })
        .await
        .expect("writer queue should remain open");

    assert_eq!(
        timeout(Duration::from_secs(1), write_complete_rx)
            .await
            .expect("writer should finish promptly")
            .expect("writer should acknowledge the message"),
        OutgoingWriteResult::Written
    );
    drop(writer_tx);
    drop(writer_control_tx);
    writer.await.expect("writer task should stop cleanly");
}

#[tokio::test]
async fn writer_disconnect_before_send_drops_acknowledgment() {
    let (writer_tx, writer_rx) = mpsc::channel(1);
    let (writer_control_tx, writer_control_rx) = mpsc::channel(1);
    let disconnect_token = CancellationToken::new();
    let failing_sink = sink::unfold((), |(), _message: AxumWebSocketMessage| async {
        Err::<(), &'static str>("disconnected")
    });
    let writer = tokio::spawn(run_websocket_outbound_loop(
        failing_sink,
        writer_rx,
        writer_control_rx,
        disconnect_token,
    ));
    let (write_complete_tx, write_complete_rx) = tokio::sync::oneshot::channel();

    writer_tx
        .send(QueuedOutgoingMessage {
            message: notification(),
            write_complete_tx: Some(write_complete_tx),
        })
        .await
        .expect("writer queue should accept message before disconnect");

    assert!(
        timeout(Duration::from_secs(1), write_complete_rx)
            .await
            .expect("failed writer should settle promptly")
            .is_err()
    );
    drop(writer_tx);
    drop(writer_control_tx);
    writer.await.expect("writer task should stop cleanly");
}
