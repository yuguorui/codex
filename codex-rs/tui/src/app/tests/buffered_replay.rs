//! Real buffered notifications must use finalized rendering without losing live stream tails.

use super::*;
use codex_app_server_protocol::ItemCompletedNotification;
use pretty_assertions::assert_eq;

fn delta(thread: &str, turn: &str, item: &str) -> ServerNotification {
    ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
        thread_id: thread.into(),
        turn_id: turn.into(),
        item_id: item.into(),
        delta: "Partial **answer**\n".into(),
    })
}

fn completed(thread: &str) -> ServerNotification {
    ServerNotification::ItemCompleted(ItemCompletedNotification {
        thread_id: thread.into(),
        turn_id: "turn".into(),
        completed_at_ms: 0,
        item: ThreadItem::AgentMessage {
            id: "answer".into(),
            text: "Final **answer**".into(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
    })
}

#[test]
fn buffered_replay_keeps_unfinished_and_unrelated_deltas_in_order() {
    let completion = completed("thread");
    let unfinished = delta("thread", "turn", "unfinished");
    let other_turn = delta("thread", "other-turn", "answer");
    let other_thread = delta("other-thread", "turn", "answer");
    let later_delta = delta("thread", "turn", "answer");
    let mut store = ThreadEventStore::new(/*capacity*/ 16);
    for event in [
        delta("thread", "turn", "answer"),
        unfinished.clone(),
        other_turn.clone(),
        other_thread.clone(),
        completion.clone(),
        later_delta.clone(),
    ] {
        store.push_notification(event);
    }
    let mut events = store.snapshot().events;
    replay_filter::omit_completed_agent_deltas(&mut events);
    let actual = events
        .into_iter()
        .map(|event| match event {
            ThreadBufferedEvent::Notification(notification) => *notification,
            other => panic!("unexpected event: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(vec![
            unfinished,
            other_turn,
            other_thread,
            completion,
            later_delta
        ])
        .unwrap()
    );
    assert_eq!(store.snapshot().events.len(), 6);
}

#[tokio::test]
async fn buffered_replay_renders_completed_text_without_streaming_again() {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let mut store = ThreadEventStore::new(/*capacity*/ 16);
    store.push_notification(delta("thread", "turn", "answer"));
    store.push_notification(completed("thread"));
    app.replay_thread_snapshot(store.snapshot(), /*resume_restored_queue*/ false);
    let mut lines = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            AppEvent::InsertHistoryCell(cell) => lines.extend(cell.display_lines(/*width*/ 80)),
            AppEvent::StartCommitAnimation | AppEvent::ConsolidateAgentMessage { .. } => {
                panic!("completed replay must not reconstruct a stream")
            }
            _ => {}
        }
    }
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text.trim(), @"
    • Final answer
    ");
}

#[test]
fn buffered_replay_preserves_text_before_interactive_requests() {
    let mut store = ThreadEventStore::new(/*capacity*/ 16);
    store.push_notification(delta("thread", "turn", "answer"));
    store.push_request(
        codex_app_server_protocol::ServerRequest::ToolRequestUserInput {
            request_id: codex_app_server_protocol::RequestId::Integer(1),
            params: codex_app_server_protocol::ToolRequestUserInputParams {
                thread_id: "thread".into(),
                turn_id: "turn".into(),
                item_id: "tool".into(),
                questions: Vec::new(),
                is_blocking: true,
                auto_resolution_ms: None,
            },
        },
    );
    store.push_notification(completed("thread"));
    let mut events = store.snapshot().events;
    let before = format!("{events:?}");
    replay_filter::omit_completed_agent_deltas(&mut events);
    assert_eq!(format!("{events:?}"), before);
}
