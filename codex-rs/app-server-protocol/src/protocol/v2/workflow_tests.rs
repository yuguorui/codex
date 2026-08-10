use std::collections::BTreeSet;

use pretty_assertions::assert_eq;

use super::WorkflowProgressItem;

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
        "attempt",
        "blocked",
        "cached",
        "durationMs",
        "error",
        "fallbackModel",
        "index",
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
