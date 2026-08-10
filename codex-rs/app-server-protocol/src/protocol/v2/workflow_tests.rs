use std::collections::BTreeSet;

use pretty_assertions::assert_eq;

use super::WorkflowApprovalArtifactReadResponse;
use super::WorkflowProgressItem;

#[test]
fn workflow_approval_artifact_response_contains_only_hash_bound_content() {
    let response = WorkflowApprovalArtifactReadResponse {
        sha256: "a".repeat(64),
        offset: 512,
        contents: "{\"tool\":\"Workflow\"}".to_string(),
        next_offset: Some(531),
    };

    assert_eq!(
        serde_json::to_value(response).unwrap(),
        serde_json::json!({
            "sha256": "a".repeat(64),
            "offset": 512,
            "contents": "{\"tool\":\"Workflow\"}",
            "nextOffset": 531,
        })
    );
}

#[test]
fn workflow_agent_json_schema_fields_match_the_camel_case_wire_format() {
    let schema = serde_json::to_value(schemars::schema_for!(WorkflowProgressItem)).unwrap();
    let variants = schema["oneOf"].as_array().unwrap();
    let agent_variant = variants
        .iter()
        .find(|variant| variant.to_string().contains("workflowAgent"))
        .unwrap();
    let mut actual = BTreeSet::new();
    collect_property_names(agent_variant, &schema, &mut actual);
    let expected = [
        "agentId",
        "activity",
        "attempt",
        "awaitingDecision",
        "blocked",
        "cached",
        "durationMs",
        "error",
        "fallbackModel",
        "index",
        "invocationId",
        "isolation",
        "label",
        "lastProgressAt",
        "model",
        "phaseIndex",
        "phaseTitle",
        "promptPreview",
        "queuedAt",
        "resultPreview",
        "skipped",
        "startedAt",
        "state",
        "tokens",
        "toolCalls",
        "type",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<BTreeSet<_>>();

    assert_eq!(actual, expected);
}

fn collect_property_names(
    schema: &serde_json::Value,
    root: &serde_json::Value,
    output: &mut BTreeSet<String>,
) {
    if let Some(reference) = schema["$ref"].as_str()
        && let Some(target) = reference
            .strip_prefix('#')
            .and_then(|path| root.pointer(path))
    {
        collect_property_names(target, root, output);
    }
    if let Some(properties) = schema["properties"].as_object() {
        output.extend(properties.keys().cloned());
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(parts) = schema[keyword].as_array() {
            for part in parts {
                collect_property_names(part, root, output);
            }
        }
    }
}
