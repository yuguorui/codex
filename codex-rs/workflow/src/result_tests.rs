use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn large_result_is_serialized_without_an_aggregate_byte_limit() {
    let result = json!({ "value": "\u{0000}".repeat(512 * 1024) });

    let serialized = serialize_workflow_result(&result).unwrap();

    assert_eq!(
        serde_json::from_str::<JsonValue>(&serialized).unwrap(),
        result
    );
}

#[test]
fn result_depth_limit_is_enforced() {
    let mut accepted = JsonValue::Null;
    for _ in 1..MAX_WORKFLOW_RESULT_DEPTH {
        accepted = JsonValue::Array(vec![accepted]);
    }
    assert!(serialize_workflow_result(&accepted).is_ok());

    let rejected = JsonValue::Array(vec![accepted]);
    assert_eq!(
        serialize_workflow_result(&rejected),
        Err(WorkflowResultLimitError::Depth)
    );
}

#[test]
fn wide_results_are_serialized_without_a_node_quota() {
    let result = JsonValue::Array(vec![JsonValue::Null; 64 * 1024]);

    assert!(serialize_workflow_result(&result).is_ok());
}
