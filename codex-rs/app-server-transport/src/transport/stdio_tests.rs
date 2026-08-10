use super::*;
use crate::OutgoingMessage;
use crate::OutgoingWriteResult;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerNotificationEnvelope;
use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use tokio::io::AsyncWrite;

struct FlushGateWriter {
    flush_started: Option<oneshot::Sender<()>>,
    flush_release: oneshot::Receiver<()>,
}

impl AsyncWrite for FlushGateWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<IoResult<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        if let Some(flush_started) = self.flush_started.take() {
            let _ = flush_started.send(());
        }
        match Pin::new(&mut self.flush_release).poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => Poll::Ready(Err(std::io::Error::new(
                ErrorKind::BrokenPipe,
                "flush gate closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }
}

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
async fn tracked_write_acknowledges_only_after_stdio_flush() {
    let (flush_started_tx, flush_started_rx) = oneshot::channel();
    let (flush_release_tx, flush_release_rx) = oneshot::channel();
    let writer = FlushGateWriter {
        flush_started: Some(flush_started_tx),
        flush_release: flush_release_rx,
    };
    let (writer_tx, writer_rx) = mpsc::channel(1);
    let writer_task = tokio::spawn(run_stdio_writer(writer, writer_rx));
    let (write_complete_tx, mut write_complete_rx) = oneshot::channel();
    writer_tx
        .send(QueuedOutgoingMessage {
            message: notification(),
            write_complete_tx: Some(write_complete_tx),
        })
        .await
        .expect("writer should accept notification");

    flush_started_rx
        .await
        .expect("writer should reach the flush boundary");
    assert!(matches!(
        write_complete_rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    flush_release_tx
        .send(())
        .expect("flush should still be waiting");
    assert_eq!(
        write_complete_rx
            .await
            .expect("flush completion should acknowledge write"),
        OutgoingWriteResult::Written
    );
    drop(writer_tx);
    writer_task.await.expect("writer task should stop cleanly");
}
