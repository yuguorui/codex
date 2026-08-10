use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_tools::TurnActivity;
use codex_tools::TurnActivityFuture;
use codex_tools::TurnActivitySubscription;

use crate::session::InputQueueActivity;
use crate::state::TurnState;

pub(super) struct CoreTurnActivitySubscription {
    activity_rx: tokio::sync::Mutex<tokio::sync::watch::Receiver<InputQueueActivity>>,
    turn_state: Option<Arc<tokio::sync::Mutex<TurnState>>>,
    observed_user_input: AtomicBool,
}

impl CoreTurnActivitySubscription {
    pub(super) fn new(
        activity_rx: tokio::sync::watch::Receiver<InputQueueActivity>,
        pending_activity: Option<InputQueueActivity>,
        turn_state: Option<Arc<tokio::sync::Mutex<TurnState>>>,
    ) -> Self {
        Self {
            activity_rx: tokio::sync::Mutex::new(activity_rx),
            turn_state,
            observed_user_input: AtomicBool::new(matches!(
                pending_activity,
                Some(InputQueueActivity::Steer)
            )),
        }
    }

    async fn latch_pending_user_input(&self) -> Option<TurnActivity> {
        let Some(turn_state) = &self.turn_state else {
            return self.observed();
        };
        if turn_state.lock().await.user_input_activity_observed() {
            self.observed_user_input.store(true, Ordering::Release);
        }
        self.observed()
    }
}

impl TurnActivitySubscription for CoreTurnActivitySubscription {
    fn observed(&self) -> Option<TurnActivity> {
        self.observed_user_input
            .load(Ordering::Acquire)
            .then_some(TurnActivity::UserInput)
    }

    fn wait<'a>(&'a self) -> TurnActivityFuture<'a> {
        Box::pin(async move {
            if let Some(activity) = self.observed() {
                return Some(activity);
            }
            if let Some(activity) = self.latch_pending_user_input().await {
                return Some(activity);
            }
            let mut activity_rx = self.activity_rx.lock().await;
            if let Some(activity) = self.observed() {
                return Some(activity);
            }
            loop {
                if activity_rx.changed().await.is_err() {
                    return self.latch_pending_user_input().await;
                }
                let _activity = *activity_rx.borrow_and_update();
                if let Some(activity) = self.latch_pending_user_input().await {
                    return Some(activity);
                }
            }
        })
    }
}

#[cfg(test)]
#[path = "extension_turn_activity_tests.rs"]
mod tests;
