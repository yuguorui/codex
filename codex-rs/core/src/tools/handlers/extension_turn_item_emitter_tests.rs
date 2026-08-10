use std::sync::Arc;

use codex_extension_items::ExtensionItem;
use codex_extension_items::image_generation::ImageGenerationItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ImageGenerationBeginEvent;
use codex_protocol::protocol::ImageGenerationEndEvent;
use codex_tools::ExtensionTurnItem;
use codex_tools::ToolApprovalReviewMode;
use codex_utils_absolute_path::test_support::PathExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;

use super::CoreTurnItemEmitter;
use crate::session::InputQueueActivity;
use crate::tools::handlers::extension_turn_activity::CoreTurnActivitySubscription;

#[tokio::test]
async fn image_generation_publication_preserves_extension_saved_path() {
    let (session, turn, rx) = crate::session::tests::make_session_and_context_with_rx().await;
    let expected_path = test_path_buf("/tmp/extension-claimed.png").abs();
    let emitter = CoreTurnItemEmitter::new(
        Arc::downgrade(&session),
        Arc::downgrade(&turn),
        "call-image".to_string(),
        codex_tools::ToolName::plain("image_generation"),
        ToolApprovalReviewMode::User,
        {
            let (_activity_tx, activity_rx) =
                tokio::sync::watch::channel(InputQueueActivity::Mailbox);
            Arc::new(CoreTurnActivitySubscription::new(
                activity_rx,
                /*pending_activity*/ None,
                /*turn_state*/ None,
            ))
        },
        Vec::new(),
    );
    let expected_started_item = ExtensionItem::ImageGeneration(ImageGenerationItem {
        id: "call-image".to_string(),
        status: "in_progress".to_string(),
        revised_prompt: None,
        result: String::new(),
        transparent_background: None,
        failure: None,
        saved_path: None,
    });
    let expected_completed_item = ExtensionItem::ImageGeneration(ImageGenerationItem {
        id: "call-image".to_string(),
        status: "completed".to_string(),
        revised_prompt: Some("A tiny blue square".to_string()),
        result: "cG5n".to_string(),
        transparent_background: Some(true),
        failure: None,
        saved_path: Some(expected_path.clone()),
    });
    codex_tools::TurnItemEmitter::emit_started(
        &emitter,
        ExtensionTurnItem {
            item: expected_started_item.clone(),
            legacy_events: vec![EventMsg::ImageGenerationBegin(ImageGenerationBeginEvent {
                call_id: "call-image".to_string(),
            })],
        },
    )
    .await;
    codex_tools::TurnItemEmitter::emit_completed(
        &emitter,
        ExtensionTurnItem {
            item: expected_completed_item.clone(),
            legacy_events: vec![EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
                call_id: "call-image".to_string(),
                status: "completed".to_string(),
                revised_prompt: Some("A tiny blue square".to_string()),
                result: "cG5n".to_string(),
                transparent_background: Some(true),
                failure: None,
                saved_path: Some(expected_path.clone()),
            })],
        },
    )
    .await;

    let started = rx.recv().await.expect("item started event");
    let EventMsg::ItemStarted(started) = started.msg else {
        panic!("expected item started event");
    };
    let TurnItem::Extension(started_item) = started.item else {
        panic!("expected extension item");
    };
    let begin = rx.recv().await.expect("legacy image start event");
    assert!(matches!(begin.msg, EventMsg::ImageGenerationBegin(_)));
    let completed = rx.recv().await.expect("item completed event");
    let EventMsg::ItemCompleted(completed) = completed.msg else {
        panic!("expected item completed event");
    };
    let TurnItem::Extension(completed_item) = completed.item else {
        panic!("expected extension item");
    };
    let end = rx.recv().await.expect("legacy image end event");
    assert!(matches!(end.msg, EventMsg::ImageGenerationEnd(_)));

    assert_eq!(started_item, expected_started_item);
    assert_eq!(completed_item, expected_completed_item);
}

#[test]
fn extension_emitter_exposes_strict_automatic_review_mode() {
    let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
    let emitter = CoreTurnItemEmitter::new(
        std::sync::Weak::new(),
        std::sync::Weak::new(),
        "call-workflow".to_string(),
        codex_tools::ToolName::plain("Workflow"),
        ToolApprovalReviewMode::StrictAutomatic,
        Arc::new(CoreTurnActivitySubscription::new(
            activity_rx,
            /*pending_activity*/ None,
            /*turn_state*/ None,
        )),
        Vec::new(),
    );
    assert_eq!(
        codex_tools::TurnItemEmitter::approval_review_mode(&emitter),
        ToolApprovalReviewMode::StrictAutomatic
    );
}
