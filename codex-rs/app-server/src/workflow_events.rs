use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowProgressNotification;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_app_server_protocol::WorkflowStatus;
use codex_app_server_protocol::WorkflowTask;
use codex_app_server_protocol::WorkflowUsage;
use codex_protocol::workflow as core;
use codex_utils_path_uri::LegacyAppPathString;
use codex_workflow_extension::WorkflowTaskSnapshot;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use crate::thread_state::ThreadStateManager;

#[derive(Clone)]
pub(crate) struct WorkflowNotificationSender {
    sender: mpsc::UnboundedSender<WorkflowNotification>,
}

enum WorkflowNotification {
    Started(core::WorkflowStartedEvent),
    Progress(core::WorkflowProgressEvent),
    Completed(core::WorkflowCompletedEvent),
}

impl WorkflowNotificationSender {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        thread_state_manager: ThreadStateManager,
    ) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut pending_progress = HashMap::new();
            let mut flush = tokio::time::interval(Duration::from_millis(250));
            flush.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    event = receiver.recv() => {
                        let Some(event) = event else {
                            flush_progress(
                                &outgoing,
                                &thread_state_manager,
                                &mut pending_progress,
                            )
                            .await;
                            break;
                        };
                        match event {
                            WorkflowNotification::Started(event) => {
                                send_notification(
                                    &outgoing,
                                    &thread_state_manager,
                                    event.thread_id,
                                    codex_app_server_protocol::ServerNotification::WorkflowStarted(
                                        started(event),
                                    ),
                                )
                                .await;
                            }
                            WorkflowNotification::Progress(event) => {
                                pending_progress.insert(
                                    (event.thread_id, event.run_id.clone()),
                                    event,
                                );
                            }
                            WorkflowNotification::Completed(event) => {
                                if let Some(progress_event) = pending_progress
                                    .remove(&(event.thread_id, event.run_id.clone()))
                                {
                                    send_progress(
                                        &outgoing,
                                        &thread_state_manager,
                                        progress_event,
                                    )
                                    .await;
                                }
                                send_notification(
                                    &outgoing,
                                    &thread_state_manager,
                                    event.thread_id,
                                    codex_app_server_protocol::ServerNotification::WorkflowCompleted(
                                        completed(event),
                                    ),
                                )
                                .await;
                            }
                        }
                    }
                    _ = flush.tick() => {
                        flush_progress(
                            &outgoing,
                            &thread_state_manager,
                            &mut pending_progress,
                        )
                        .await;
                    }
                }
            }
        });
        Self { sender }
    }

    pub(crate) fn started(&self, event: core::WorkflowStartedEvent) {
        self.send(WorkflowNotification::Started(event));
    }

    pub(crate) fn progress(&self, event: core::WorkflowProgressEvent) {
        self.send(WorkflowNotification::Progress(event));
    }

    pub(crate) fn completed(&self, event: core::WorkflowCompletedEvent) {
        self.send(WorkflowNotification::Completed(event));
    }

    fn send(&self, event: WorkflowNotification) {
        if self.sender.send(event).is_err() {
            tracing::warn!("workflow notification queue is closed");
        }
    }
}

async fn flush_progress(
    outgoing: &Arc<OutgoingMessageSender>,
    thread_state_manager: &ThreadStateManager,
    pending: &mut HashMap<(codex_protocol::ThreadId, String), core::WorkflowProgressEvent>,
) {
    for (_, event) in std::mem::take(pending) {
        send_progress(outgoing, thread_state_manager, event).await;
    }
}

async fn send_progress(
    outgoing: &Arc<OutgoingMessageSender>,
    thread_state_manager: &ThreadStateManager,
    event: core::WorkflowProgressEvent,
) {
    send_notification(
        outgoing,
        thread_state_manager,
        event.thread_id,
        codex_app_server_protocol::ServerNotification::WorkflowProgress(progress(event)),
    )
    .await;
}

async fn send_notification(
    outgoing: &Arc<OutgoingMessageSender>,
    thread_state_manager: &ThreadStateManager,
    thread_id: codex_protocol::ThreadId,
    notification: codex_app_server_protocol::ServerNotification,
) {
    let subscribed_connection_ids = thread_state_manager
        .subscribed_connection_ids(thread_id)
        .await;
    ThreadScopedOutgoingMessageSender::new(
        Arc::clone(outgoing),
        subscribed_connection_ids,
        thread_id,
    )
    .send_server_notification(notification)
    .await;
}

pub(crate) fn started(event: core::WorkflowStartedEvent) -> WorkflowStartedNotification {
    WorkflowStartedNotification {
        thread_id: event.thread_id.to_string(),
        turn_id: event.turn_id,
        task_id: event.task_id,
        run_id: event.run_id,
        workflow_name: event.workflow_name,
        title: event.title,
        summary: event.summary,
        transcript_dir: LegacyAppPathString::from_abs_path(&event.transcript_dir),
        script_path: LegacyAppPathString::from_abs_path(&event.script_path),
        started_at: event.started_at,
    }
}

pub(crate) fn progress(event: core::WorkflowProgressEvent) -> WorkflowProgressNotification {
    WorkflowProgressNotification {
        thread_id: event.thread_id.to_string(),
        turn_id: event.turn_id,
        task_id: event.task_id,
        run_id: event.run_id,
        progress: event.progress.into_iter().map(Into::into).collect(),
        usage: usage(event.usage),
    }
}

pub(crate) fn completed(event: core::WorkflowCompletedEvent) -> WorkflowCompletedNotification {
    WorkflowCompletedNotification {
        thread_id: event.thread_id.to_string(),
        turn_id: event.turn_id,
        task_id: event.task_id,
        run_id: event.run_id,
        workflow_name: event.workflow_name,
        status: status(event.status),
        summary: event.summary,
        output_file: LegacyAppPathString::from_abs_path(&event.output_file),
        error: event.error,
        failures: event.failures,
        usage: usage(event.usage),
        completed_at: event.completed_at,
    }
}

pub(crate) fn task(snapshot: WorkflowTaskSnapshot) -> WorkflowTask {
    WorkflowTask {
        thread_id: snapshot.thread_id,
        turn_id: snapshot.turn_id,
        task_id: snapshot.task_id,
        run_id: snapshot.run_id,
        workflow_name: snapshot.workflow_name,
        title: snapshot.title,
        status: status(snapshot.status),
        summary: snapshot.summary,
        transcript_dir: LegacyAppPathString::from_abs_path(&snapshot.transcript_dir),
        script_path: LegacyAppPathString::from_abs_path(&snapshot.script_path),
        output_file: LegacyAppPathString::from_abs_path(&snapshot.output_file),
        progress: snapshot.progress.into_iter().map(Into::into).collect(),
        progress_version: snapshot.progress_version,
        usage: usage(snapshot.usage),
        failures: snapshot.failures,
        error: snapshot.error,
        started_at: snapshot.started_at,
        completed_at: snapshot.completed_at,
    }
}

fn status(status: core::WorkflowTaskStatus) -> WorkflowStatus {
    match status {
        core::WorkflowTaskStatus::Pending => WorkflowStatus::Pending,
        core::WorkflowTaskStatus::Running => WorkflowStatus::Running,
        core::WorkflowTaskStatus::Completed => WorkflowStatus::Completed,
        core::WorkflowTaskStatus::Failed => WorkflowStatus::Failed,
        core::WorkflowTaskStatus::Paused => WorkflowStatus::Paused,
        core::WorkflowTaskStatus::Killed => WorkflowStatus::Killed,
    }
}

fn usage(usage: core::WorkflowUsage) -> WorkflowUsage {
    WorkflowUsage {
        total_tokens: usage.total_tokens,
        tool_uses: usage.tool_uses,
        duration_ms: usage.duration_ms,
        agent_count: usage.agent_count,
    }
}
