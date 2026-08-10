use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowProgressNotification;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_app_server_protocol::WorkflowStatus;
use codex_app_server_protocol::WorkflowTask;
use codex_app_server_protocol::WorkflowUsage;
use codex_extension_api::ExtensionEventDelivery;
use codex_protocol::workflow as core;
use codex_utils_path_uri::LegacyAppPathString;
use codex_workflow_extension::WorkflowTaskSnapshot;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingDelivery;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::TRACKED_FANOUT_CAPACITY;
use crate::outgoing_message::TRACKED_WRITE_ACK_TIMEOUT;
use crate::thread_state::ThreadStateManager;

#[derive(Clone)]
pub(crate) struct WorkflowNotificationSender {
    inner: Arc<WorkflowNotificationInner>,
}

struct WorkflowNotificationInner {
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
    state: Mutex<WorkflowNotificationState>,
    write_ack_timeout: std::time::Duration,
}

enum LifecycleNotification {
    Started(core::WorkflowStartedEvent),
    Completed(core::WorkflowCompletedEvent),
}

struct PendingLifecycleNotification {
    notification: LifecycleNotification,
    delivered: oneshot::Sender<ExtensionEventDelivery>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WorkflowExecutionKey {
    thread_id: codex_protocol::ThreadId,
    run_id: String,
    task_id: String,
}

#[derive(Default)]
struct WorkflowNotificationState {
    lanes: HashMap<WorkflowExecutionKey, WorkflowExecutionLane>,
    order: VecDeque<WorkflowExecutionKey>,
    lifecycle_count: usize,
    deliveries: HashMap<String, LifecycleFanoutState>,
    delivery_order: VecDeque<String>,
}

#[derive(Default)]
struct WorkflowExecutionLane {
    lifecycle: VecDeque<PendingLifecycleNotification>,
    progress: Option<core::WorkflowProgressEvent>,
    progress_resync_required: bool,
    delivering: bool,
}

enum WorkflowDeliveryAction {
    Lifecycle {
        pending: PendingLifecycleNotification,
        progress_resync_required: bool,
    },
    Progress(core::WorkflowProgressEvent),
}

struct ProgressDelivery {
    complete: bool,
    wrote: bool,
}

#[derive(Default)]
struct LifecycleFanoutState {
    settled_through: Option<ConnectionId>,
    pending: BTreeSet<ConnectionId>,
    batch_end: Option<ConnectionId>,
    has_more: bool,
    retry_after_batch: bool,
    complete: bool,
    progress_resync_required: Option<bool>,
}

struct LifecycleFanoutSnapshot {
    settled_through: Option<ConnectionId>,
    pending: Vec<ConnectionId>,
    batch_loaded: bool,
    has_more: bool,
    complete: bool,
}

// These bounds include active writes, queued lifecycle events, and coalesced
// progress across every workflow execution in this app-server process.
const WORKFLOW_LIFECYCLE_BUFFER_CAPACITY: usize = 64;
const WORKFLOW_EXECUTION_BUFFER_CAPACITY: usize = 256;
const WORKFLOW_DELIVERY_RECORD_CAPACITY: usize = 128;

impl WorkflowExecutionKey {
    fn from_progress(event: &core::WorkflowProgressEvent) -> Self {
        Self {
            thread_id: event.thread_id,
            run_id: event.run_id.clone(),
            task_id: event.task_id.clone(),
        }
    }

    fn from_completed(event: &core::WorkflowCompletedEvent) -> Self {
        Self {
            thread_id: event.thread_id,
            run_id: event.run_id.clone(),
            task_id: event.task_id.clone(),
        }
    }

    fn from_lifecycle(notification: &LifecycleNotification) -> Self {
        match notification {
            LifecycleNotification::Started(event) => Self {
                thread_id: event.thread_id,
                run_id: event.run_id.clone(),
                task_id: event.task_id.clone(),
            },
            LifecycleNotification::Completed(event) => Self::from_completed(event),
        }
    }
}

impl WorkflowNotificationState {
    fn insert(&mut self, event: core::WorkflowProgressEvent) {
        let key = WorkflowExecutionKey::from_progress(&event);
        if let Some(lane) = self.lanes.get_mut(&key) {
            lane.progress = Some(event);
            self.touch(&key);
            return;
        }
        if !self.make_room_for_lane() {
            return;
        }
        self.order.push_back(key.clone());
        self.lanes.insert(
            key,
            WorkflowExecutionLane {
                progress: Some(event),
                ..Default::default()
            },
        );
    }

    fn enqueue_lifecycle(
        &mut self,
        pending: PendingLifecycleNotification,
    ) -> Result<(WorkflowExecutionKey, bool), PendingLifecycleNotification> {
        if self.lifecycle_count >= WORKFLOW_LIFECYCLE_BUFFER_CAPACITY {
            return Err(pending);
        }
        let key = WorkflowExecutionKey::from_lifecycle(&pending.notification);
        let missing_completed_lane =
            matches!(&pending.notification, LifecycleNotification::Completed(_))
                && !self.lanes.contains_key(&key);
        if !self.lanes.contains_key(&key) {
            if !self.make_room_for_lane() {
                return Err(pending);
            }
            self.order.push_back(key.clone());
            self.lanes.insert(
                key.clone(),
                WorkflowExecutionLane {
                    progress_resync_required: missing_completed_lane,
                    ..Default::default()
                },
            );
        }
        self.lifecycle_count += 1;
        let lane = self.lanes.get_mut(&key).expect("lane was inserted above");
        lane.lifecycle.push_back(pending);
        let should_spawn = !lane.delivering;
        lane.delivering = true;
        self.touch(&key);
        Ok((key, should_spawn))
    }

    fn next_action(&mut self, key: &WorkflowExecutionKey) -> Option<WorkflowDeliveryAction> {
        let lane = self.lanes.get_mut(key)?;
        if matches!(
            lane.lifecycle.front().map(|pending| &pending.notification),
            Some(LifecycleNotification::Started(_))
        ) {
            let pending = lane.lifecycle.pop_front().expect("front was present");
            return Some(WorkflowDeliveryAction::Lifecycle {
                pending,
                progress_resync_required: lane.progress_resync_required,
            });
        }
        if let Some(progress) = lane.progress.take() {
            return Some(WorkflowDeliveryAction::Progress(progress));
        }
        if let Some(pending) = lane.lifecycle.pop_front() {
            return Some(WorkflowDeliveryAction::Lifecycle {
                pending,
                progress_resync_required: lane.progress_resync_required,
            });
        }
        lane.delivering = false;
        None
    }

    fn finish_lifecycle(
        &mut self,
        key: &WorkflowExecutionKey,
        terminal: bool,
        delivered: bool,
    ) -> Vec<PendingLifecycleNotification> {
        self.lifecycle_count = self.lifecycle_count.saturating_sub(1);
        if terminal || !delivered {
            return self.remove_lane(key);
        }
        Vec::new()
    }

    fn mark_progress_dropped(&mut self, key: &WorkflowExecutionKey) {
        if let Some(lane) = self.lanes.get_mut(key) {
            lane.progress_resync_required = true;
        }
    }

    fn mark_progress_delivered(&mut self, key: &WorkflowExecutionKey) {
        if let Some(lane) = self.lanes.get_mut(key) {
            lane.progress_resync_required = false;
        }
    }

    fn lifecycle_delivery_snapshot(
        &mut self,
        idempotency_key: &str,
    ) -> Option<LifecycleFanoutSnapshot> {
        if !self.deliveries.contains_key(idempotency_key) {
            while self.deliveries.len() >= WORKFLOW_DELIVERY_RECORD_CAPACITY {
                let position = self.delivery_order.iter().position(|key| {
                    self.deliveries
                        .get(key)
                        .is_some_and(|delivery| delivery.complete)
                })?;
                let key = self.delivery_order.remove(position)?;
                self.deliveries.remove(&key);
            }
            self.deliveries
                .insert(idempotency_key.to_string(), LifecycleFanoutState::default());
        }
        self.delivery_order.retain(|key| key != idempotency_key);
        self.delivery_order.push_back(idempotency_key.to_string());
        let delivery = self.deliveries.get(idempotency_key)?;
        Some(LifecycleFanoutSnapshot {
            settled_through: delivery.settled_through,
            pending: delivery.pending.iter().copied().collect(),
            batch_loaded: delivery.batch_end.is_some(),
            has_more: delivery.has_more,
            complete: delivery.complete,
        })
    }

    fn set_lifecycle_batch(
        &mut self,
        idempotency_key: &str,
        connection_ids: Vec<ConnectionId>,
        next_after: Option<ConnectionId>,
    ) {
        let Some(delivery) = self.deliveries.get_mut(idempotency_key) else {
            return;
        };
        if connection_ids.is_empty() {
            delivery.batch_end = None;
            delivery.pending.clear();
            delivery.has_more = false;
            if delivery.settled_through.is_some() {
                delivery.complete = true;
            }
            return;
        }
        delivery.complete = false;
        delivery.batch_end = connection_ids.last().copied();
        delivery.pending = connection_ids.into_iter().collect();
        delivery.has_more = next_after.is_some();
    }

    fn lifecycle_resync_required(&mut self, idempotency_key: &str, requested: bool) -> bool {
        let Some(delivery) = self.deliveries.get_mut(idempotency_key) else {
            return requested;
        };
        *delivery.progress_resync_required.get_or_insert(requested)
    }

    fn retain_lifecycle_pending(
        &mut self,
        idempotency_key: &str,
        connection_ids: Vec<ConnectionId>,
    ) {
        let Some(delivery) = self.deliveries.get_mut(idempotency_key) else {
            return;
        };
        delivery.retry_after_batch |= connection_ids.len() < delivery.pending.len();
        delivery.pending = connection_ids.into_iter().collect();
        Self::settle_lifecycle_batch(delivery);
    }

    fn record_lifecycle_delivery(
        &mut self,
        idempotency_key: &str,
        outcomes: impl IntoIterator<Item = (ConnectionId, OutgoingDelivery)>,
    ) -> bool {
        let Some(delivery) = self.deliveries.get_mut(idempotency_key) else {
            return false;
        };
        for (connection_id, outcome) in outcomes {
            if matches!(
                outcome,
                OutgoingDelivery::Written | OutgoingDelivery::NotTarget
            ) {
                delivery.pending.remove(&connection_id);
            }
        }
        Self::settle_lifecycle_batch(delivery);
        delivery.complete
    }

    fn settle_lifecycle_batch(delivery: &mut LifecycleFanoutState) {
        if !delivery.pending.is_empty() || delivery.batch_end.is_none() {
            return;
        }
        delivery.settled_through = delivery.batch_end.take();
        if delivery.has_more || delivery.retry_after_batch {
            delivery.has_more = true;
            delivery.retry_after_batch = false;
        } else {
            delivery.complete = true;
        }
    }

    fn make_room_for_lane(&mut self) -> bool {
        while self.lanes.len() >= WORKFLOW_EXECUTION_BUFFER_CAPACITY {
            let Some(position) = self.order.iter().position(|key| {
                self.lanes
                    .get(key)
                    .is_some_and(|lane| !lane.delivering && lane.lifecycle.is_empty())
            }) else {
                return false;
            };
            let Some(key) = self.order.remove(position) else {
                return false;
            };
            self.lanes.remove(&key);
        }
        true
    }

    fn remove_lane(&mut self, key: &WorkflowExecutionKey) -> Vec<PendingLifecycleNotification> {
        self.order.retain(|pending| pending != key);
        let Some(lane) = self.lanes.remove(key) else {
            return Vec::new();
        };
        self.lifecycle_count = self.lifecycle_count.saturating_sub(lane.lifecycle.len());
        lane.lifecycle.into_iter().collect()
    }

    fn touch(&mut self, key: &WorkflowExecutionKey) {
        self.order.retain(|pending| pending != key);
        self.order.push_back(key.clone());
    }
}

impl WorkflowNotificationInner {
    fn progress(self: &Arc<Self>, event: core::WorkflowProgressEvent) {
        let key = WorkflowExecutionKey::from_progress(&event);
        let should_spawn = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.insert(event);
            let Some(lane) = state.lanes.get_mut(&key) else {
                return;
            };
            let should_spawn = !lane.delivering;
            lane.delivering = true;
            should_spawn
        };
        if should_spawn {
            self.spawn_lane(key);
        }
    }

    fn spawn_lane(self: &Arc<Self>, key: WorkflowExecutionKey) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner.deliver_lane(key).await;
        });
    }

    async fn deliver_lane(self: Arc<Self>, key: WorkflowExecutionKey) {
        loop {
            let action = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .next_action(&key);
            let Some(action) = action else {
                return;
            };
            match action {
                WorkflowDeliveryAction::Progress(event) => {
                    let delivery = self.deliver_progress(event).await;
                    let mut state = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if delivery.complete && delivery.wrote {
                        state.mark_progress_delivered(&key);
                    } else {
                        state.mark_progress_dropped(&key);
                    }
                }
                WorkflowDeliveryAction::Lifecycle {
                    pending,
                    progress_resync_required,
                } => {
                    let terminal =
                        matches!(&pending.notification, LifecycleNotification::Completed(_));
                    let idempotency_key = lifecycle_idempotency_key(&pending.notification);
                    let delivery = self
                        .deliver_lifecycle(pending.notification, progress_resync_required)
                        .await;
                    let acknowledged = delivery == OutgoingDelivery::Written;
                    let result = if acknowledged {
                        ExtensionEventDelivery::Acknowledged { idempotency_key }
                    } else {
                        ExtensionEventDelivery::Retryable { idempotency_key }
                    };
                    let _ = pending.delivered.send(result.clone());
                    let abandoned = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .finish_lifecycle(&key, terminal, acknowledged);
                    for pending in abandoned {
                        let idempotency_key = lifecycle_idempotency_key(&pending.notification);
                        let _ = pending
                            .delivered
                            .send(ExtensionEventDelivery::Retryable { idempotency_key });
                    }
                    if terminal || !acknowledged {
                        return;
                    }
                }
            }
        }
    }

    async fn deliver_progress(&self, event: core::WorkflowProgressEvent) -> ProgressDelivery {
        let thread_id = event.thread_id;
        let page = self
            .thread_state_manager
            .subscribed_connection_page(thread_id, /*after*/ None, TRACKED_FANOUT_CAPACITY)
            .await;
        let delivery = self
            .outgoing
            .try_send_server_notification_to_connections_with_timeout(
                &page.connection_ids,
                codex_app_server_protocol::ServerNotification::WorkflowProgress(progress(event)),
                self.write_ack_timeout,
            )
            .await;
        let wrote = delivery
            .subscribers
            .iter()
            .any(|subscriber| subscriber.outcome == OutgoingDelivery::Written);
        let complete = page.next_after.is_none()
            && !delivery.truncated
            && delivery
                .subscribers
                .iter()
                .all(|subscriber| subscriber.outcome != OutgoingDelivery::Retryable);
        ProgressDelivery { complete, wrote }
    }

    async fn deliver_lifecycle(
        &self,
        notification: LifecycleNotification,
        progress_resync_required: bool,
    ) -> OutgoingDelivery {
        let idempotency_key = lifecycle_idempotency_key(&notification);
        let thread_id = WorkflowExecutionKey::from_lifecycle(&notification).thread_id;
        let deadline = Instant::now() + self.write_ack_timeout;
        let snapshot = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(snapshot) = state.lifecycle_delivery_snapshot(&idempotency_key) else {
                return OutgoingDelivery::Retryable;
            };
            snapshot
        };
        if snapshot.complete {
            return OutgoingDelivery::Written;
        }
        let wire_notification = match notification {
            LifecycleNotification::Started(event) => {
                codex_app_server_protocol::ServerNotification::WorkflowStarted(started(
                    event,
                    idempotency_key.clone(),
                ))
            }
            LifecycleNotification::Completed(event) => {
                let progress_resync_required = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .lifecycle_resync_required(&idempotency_key, progress_resync_required);
                codex_app_server_protocol::ServerNotification::WorkflowCompleted(completed(
                    event,
                    idempotency_key.clone(),
                    progress_resync_required,
                ))
            }
        };

        loop {
            if Instant::now() >= deadline {
                return OutgoingDelivery::Retryable;
            }
            let mut snapshot = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .lifecycle_delivery_snapshot(&idempotency_key)
                .expect("delivery record should remain resident while incomplete");
            if snapshot.complete {
                return OutgoingDelivery::Written;
            }
            if !snapshot.pending.is_empty() {
                let active = self
                    .thread_state_manager
                    .retain_subscribed_connections(thread_id, &snapshot.pending)
                    .await;
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain_lifecycle_pending(&idempotency_key, active);
                snapshot = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .lifecycle_delivery_snapshot(&idempotency_key)
                    .expect("delivery record should remain resident while incomplete");
                if snapshot.complete {
                    return OutgoingDelivery::Written;
                }
            }
            if snapshot.pending.is_empty() && !snapshot.batch_loaded {
                let page = self
                    .thread_state_manager
                    .subscribed_connection_page(
                        thread_id,
                        snapshot.settled_through,
                        TRACKED_FANOUT_CAPACITY,
                    )
                    .await;
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .set_lifecycle_batch(&idempotency_key, page.connection_ids, page.next_after);
                snapshot = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .lifecycle_delivery_snapshot(&idempotency_key)
                    .expect("delivery record should remain resident while incomplete");
                if snapshot.complete {
                    return OutgoingDelivery::Written;
                }
                if snapshot.pending.is_empty() {
                    return OutgoingDelivery::Retryable;
                }
            }
            let delivery = self
                .outgoing
                .try_send_server_notification_to_connections_until(
                    &snapshot.pending,
                    wire_notification.clone(),
                    deadline,
                )
                .await;
            let retryable = delivery.truncated
                || delivery
                    .subscribers
                    .iter()
                    .any(|subscriber| subscriber.outcome == OutgoingDelivery::Retryable);
            let complete = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record_lifecycle_delivery(
                    &idempotency_key,
                    delivery
                        .subscribers
                        .into_iter()
                        .map(|subscriber| (subscriber.connection_id, subscriber.outcome)),
                );
            if complete {
                return OutgoingDelivery::Written;
            }
            if retryable {
                return OutgoingDelivery::Retryable;
            }
            let has_more = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .lifecycle_delivery_snapshot(&idempotency_key)
                .is_some_and(|snapshot| snapshot.has_more);
            if !has_more {
                return OutgoingDelivery::Retryable;
            }
            tokio::task::yield_now().await;
        }
    }
}

impl WorkflowNotificationSender {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        thread_state_manager: ThreadStateManager,
    ) -> Self {
        Self {
            inner: Arc::new(WorkflowNotificationInner {
                outgoing,
                thread_state_manager,
                state: Mutex::new(WorkflowNotificationState::default()),
                write_ack_timeout: TRACKED_WRITE_ACK_TIMEOUT,
            }),
        }
    }

    #[cfg(test)]
    fn new_with_write_ack_timeout(
        outgoing: Arc<OutgoingMessageSender>,
        thread_state_manager: ThreadStateManager,
        write_ack_timeout: std::time::Duration,
    ) -> Self {
        Self {
            inner: Arc::new(WorkflowNotificationInner {
                outgoing,
                thread_state_manager,
                state: Mutex::new(WorkflowNotificationState::default()),
                write_ack_timeout,
            }),
        }
    }

    pub(crate) async fn started(
        &self,
        event: core::WorkflowStartedEvent,
    ) -> ExtensionEventDelivery {
        self.send_lifecycle(LifecycleNotification::Started(event))
            .await
    }

    pub(crate) fn progress(&self, event: core::WorkflowProgressEvent) {
        self.inner.progress(event);
    }

    pub(crate) async fn completed(
        &self,
        event: core::WorkflowCompletedEvent,
    ) -> ExtensionEventDelivery {
        self.send_lifecycle(LifecycleNotification::Completed(event))
            .await
    }

    async fn send_lifecycle(&self, notification: LifecycleNotification) -> ExtensionEventDelivery {
        let idempotency_key = lifecycle_idempotency_key(&notification);
        let (delivered, delivered_rx) = oneshot::channel();
        let enqueue_result = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enqueue_lifecycle(PendingLifecycleNotification {
                notification,
                delivered,
            });
        let (key, should_spawn) = match enqueue_result {
            Ok(enqueued) => enqueued,
            Err(_) => {
                return ExtensionEventDelivery::Retryable { idempotency_key };
            }
        };
        if should_spawn {
            self.inner.spawn_lane(key);
        }
        match delivered_rx.await {
            Ok(delivery) => delivery,
            Err(_) => ExtensionEventDelivery::Retryable { idempotency_key },
        }
    }
}

fn lifecycle_idempotency_key(notification: &LifecycleNotification) -> String {
    match notification {
        LifecycleNotification::Started(event) => format!(
            "workflow/started/{}/{}/{}",
            event.thread_id, event.run_id, event.task_id
        ),
        LifecycleNotification::Completed(event) => format!(
            "workflow/completed/{}/{}/{}",
            event.thread_id, event.run_id, event.task_id
        ),
    }
}

pub(crate) fn started(
    event: core::WorkflowStartedEvent,
    delivery_key: String,
) -> WorkflowStartedNotification {
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
        delivery_key,
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

pub(crate) fn completed(
    event: core::WorkflowCompletedEvent,
    delivery_key: String,
    progress_resync_required: bool,
) -> WorkflowCompletedNotification {
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
        delivery_key,
        progress_resync_required,
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

#[cfg(test)]
#[path = "workflow_events_tests.rs"]
mod tests;
