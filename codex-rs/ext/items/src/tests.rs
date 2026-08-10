use pretty_assertions::assert_eq;
use serde_json::json;
use ts_rs::TS;

use super::ExtensionItem;
use super::image_generation::ImageGenerationItem;
use super::sleep::SleepItem;
use super::web_search::WebSearchAction;
use super::web_search::WebSearchItem;
use super::workflow::WorkflowInputAnalysisItem;
use super::workflow::WorkflowResultReadItem;
use super::workflow::WorkflowResultReadStatus;

#[test]
fn workflow_activity_items_preserve_focused_wire_shapes() {
    let items = [
        ExtensionItem::WorkflowInputAnalysis(WorkflowInputAnalysisItem {
            id: "analyze-1".to_string(),
        }),
        ExtensionItem::WorkflowResultRead(WorkflowResultReadItem {
            id: "read-1".to_string(),
            run_id: Some("wf_123".to_string()),
            status: WorkflowResultReadStatus::InProgress,
        }),
    ];

    assert_eq!(
        items.map(|item| serde_json::to_value(item).unwrap()),
        [
            json!({"kind": "workflow.input_analysis", "id": "analyze-1"}),
            json!({
                "kind": "workflow.result_read",
                "id": "read-1",
                "runId": "wf_123",
                "status": "inProgress",
            }),
        ]
    );
}

#[test]
fn workflow_result_read_item_preserves_terminal_states() {
    let items = [
        WorkflowResultReadItem {
            id: "read-completed".to_string(),
            run_id: Some("wf_completed".to_string()),
            status: WorkflowResultReadStatus::Completed,
        },
        WorkflowResultReadItem {
            id: "read-failed".to_string(),
            run_id: None,
            status: WorkflowResultReadStatus::Failed,
        },
    ];

    assert_eq!(
        items.map(|item| serde_json::to_value(item).unwrap()),
        [
            json!({
                "id": "read-completed",
                "runId": "wf_completed",
                "status": "completed",
            }),
            json!({
                "id": "read-failed",
                "runId": null,
                "status": "failed",
            }),
        ]
    );
}

fn completed_image_generation_item() -> ExtensionItem {
    ExtensionItem::ImageGeneration(ImageGenerationItem {
        id: "image-1".to_string(),
        status: "completed".to_string(),
        revised_prompt: Some("A blue square".to_string()),
        result: "cG5n".to_string(),
        transparent_background: None,
        failure: None,
        saved_path: None,
    })
}

#[test]
fn image_generation_item_preserves_stable_wire_shape() {
    let item = completed_image_generation_item();
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "image_gen.generation",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": "A blue square",
            "result": "cG5n",
            "transparentBackground": null,
            "failure": null,
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(json!({
            "kind": "image_gen.generation",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": "A blue square",
            "result": "cG5n",
        }))
        .expect("deserialize legacy image-generation item without transparency metadata"),
        item
    );
}

#[test]
fn image_generation_item_preserves_authoritative_transparency() {
    let ExtensionItem::ImageGeneration(mut image) = completed_image_generation_item() else {
        panic!("expected image-generation item");
    };
    image.transparent_background = Some(true);
    let item = ExtensionItem::ImageGeneration(image);
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "image_gen.generation",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": "A blue square",
            "result": "cG5n",
            "transparentBackground": true,
            "failure": null,
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
}

#[test]
fn image_generation_transparency_is_optional_in_typescript() {
    assert!(
        ImageGenerationItem::inline().contains("transparentBackground?: boolean"),
        "image-generation transparency must remain optional for existing TypeScript clients"
    );
}

#[test]
fn web_search_item_preserves_stable_wire_shape() {
    let item = ExtensionItem::WebSearch(WebSearchItem {
        id: "search-1".to_string(),
        query: "docs".to_string(),
        action: Some(WebSearchAction::Search {
            query: Some("docs".to_string()),
            queries: None,
        }),
        results: None,
    });
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "web.search",
            "id": "search-1",
            "query": "docs",
            "action": {
                "type": "search",
                "query": "docs",
                "queries": null,
            },
            "results": null,
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(json!({
            "kind": "web.search",
            "id": "search-1",
            "query": "docs",
            "action": {
                "type": "search",
                "query": "docs",
                "queries": null,
            },
        }))
        .expect("deserialize legacy extension item without results"),
        item
    );
}

#[test]
fn sleep_item_preserves_stable_wire_shape() {
    let item = ExtensionItem::Sleep(SleepItem {
        id: "sleep-1".to_string(),
        duration_ms: 1_000,
    });
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "clock.sleep",
            "id": "sleep-1",
            "durationMs": 1_000,
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
}

#[test]
fn unknown_extension_kind_is_rejected() {
    let value = json!({
        "kind": "image_gen.unknown",
        "id": "image-1",
    });

    assert!(serde_json::from_value::<ExtensionItem>(value).is_err());
}

#[test]
fn malformed_known_extension_payload_is_rejected() {
    let value = json!({
        "kind": "image_gen.generation",
        "id": "image-1",
        "status": "completed",
    });

    assert!(serde_json::from_value::<ExtensionItem>(value).is_err());
}
