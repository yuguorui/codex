use serde_json::Value as JsonValue;

pub const MAX_WORKFLOW_RESULT_DEPTH: usize = 64;
const WORKFLOW_RESULT_GUIDANCE: &str = "return a shallower workflow result";

/// Describes why a workflow result cannot cross the runtime result boundary.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkflowResultLimitError {
    #[error("WorkflowResultLimitError: {WORKFLOW_RESULT_GUIDANCE}")]
    Depth,
    #[error("WorkflowResultSerializationError: failed to serialize workflow result: {0}")]
    Serialization(String),
}

/// Serializes a workflow result after enforcing structural complexity limits.
pub fn serialize_workflow_result(result: &JsonValue) -> Result<String, WorkflowResultLimitError> {
    preflight_result(result)?;
    serde_json::to_string(result)
        .map_err(|error| WorkflowResultLimitError::Serialization(error.to_string()))
}

fn preflight_result(result: &JsonValue) -> Result<(), WorkflowResultLimitError> {
    let mut stack = vec![(result, 1_usize)];
    while let Some((value, depth)) = stack.pop() {
        if depth > MAX_WORKFLOW_RESULT_DEPTH {
            return Err(WorkflowResultLimitError::Depth);
        }
        match value {
            JsonValue::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth.saturating_add(1))));
            }
            JsonValue::Object(values) => {
                for value in values.values() {
                    stack.push((value, depth.saturating_add(1)));
                }
            }
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "result_tests.rs"]
mod tests;
