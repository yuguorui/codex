use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;

const JSON_POINTER_MAX_BYTES: usize = 512;

#[derive(Clone, Debug)]
pub(crate) struct ProjectedWorkflowResult {
    pub(crate) json_pointer: String,
    pub(crate) value: JsonValue,
    pub(crate) serialized: String,
    pub(crate) sha256: String,
}

pub(crate) fn project_workflow_result(
    serialized: &str,
    json_pointer: &str,
) -> Result<ProjectedWorkflowResult, String> {
    if json_pointer.len() > JSON_POINTER_MAX_BYTES {
        return Err("choose a jsonPointer no longer than 512 UTF-8 bytes".to_string());
    }
    validate_json_pointer(json_pointer)?;
    let value: JsonValue = serde_json::from_str(serialized)
        .map_err(|error| format!("workflow result artifact is not valid JSON: {error}"))?;
    let Some(value) = value.pointer(json_pointer) else {
        return Err(format!(
            "jsonPointer {json_pointer:?} does not select a value in the workflow result"
        ));
    };
    let value = value.clone();
    let serialized = serde_json::to_string(&value)
        .map_err(|error| format!("failed to serialize the projected workflow result: {error}"))?;
    Ok(ProjectedWorkflowResult {
        json_pointer: json_pointer.to_string(),
        sha256: format!("{:x}", Sha256::digest(serialized.as_bytes())),
        value,
        serialized,
    })
}

fn validate_json_pointer(json_pointer: &str) -> Result<(), String> {
    if json_pointer.is_empty() {
        return Ok(());
    }
    if !json_pointer.starts_with('/') {
        return Err("jsonPointer must be empty or start with '/'".to_string());
    }
    for token in json_pointer[1..].split('/') {
        let mut escaped = false;
        for character in token.chars() {
            if escaped {
                if !matches!(character, '0' | '1') {
                    return Err(
                        "jsonPointer escape sequences must use ~0 or ~1 instead of a raw '~'"
                            .to_string(),
                    );
                }
                escaped = false;
            } else if character == '~' {
                escaped = true;
            }
        }
        if escaped {
            return Err("jsonPointer ends with an incomplete ~0 or ~1 escape".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "workflow_result_projection_tests.rs"]
mod tests;
