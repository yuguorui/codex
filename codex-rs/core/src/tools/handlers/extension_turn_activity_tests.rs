use std::sync::Arc;

use codex_tools::TurnActivitySubscription;
use pretty_assertions::assert_eq;
use std::task::Poll;
use tokio::sync::Mutex;

use super::CoreTurnActivitySubscription;
use crate::session::InputQueueActivity;

#[tokio::test]
async fn non_user_wake_after_wait_entry_does_not_interrupt_and_user_input_stays_latched() {
    let turn_state = Arc::new(Mutex::new(crate::state::TurnState::default()));
    let (activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
    let subscription = CoreTurnActivitySubscription::new(
        activity_rx,
        /*pending_activity*/ None,
        Some(Arc::clone(&turn_state)),
    );
    let wait = subscription.wait();
    tokio::pin!(wait);

    assert!(matches!(futures::poll!(&mut wait), Poll::Pending));

    activity_tx.send_replace(InputQueueActivity::Steer);
    assert!(matches!(futures::poll!(&mut wait), Poll::Pending));

    turn_state.lock().await.mark_user_input_activity_observed();
    activity_tx.send_replace(InputQueueActivity::Steer);
    assert_eq!(wait.await, Some(codex_tools::TurnActivity::UserInput));
    assert_eq!(
        subscription.observed(),
        Some(codex_tools::TurnActivity::UserInput)
    );
    assert_eq!(
        subscription.wait().await,
        Some(codex_tools::TurnActivity::UserInput)
    );
}

#[tokio::test]
async fn reports_already_pending_user_input() {
    let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
    let subscription = CoreTurnActivitySubscription::new(
        activity_rx,
        Some(InputQueueActivity::Steer),
        /*turn_state*/ None,
    );

    assert_eq!(
        subscription.wait().await,
        Some(codex_tools::TurnActivity::UserInput)
    );
}
