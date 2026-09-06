use codex_tools::ToolSpec;

use super::WORKFLOW_TOOL_NAME;
use super::workflow_tool_spec;

#[test]
fn workflow_tool_text_keeps_core_contracts_focused() {
    let ToolSpec::Function(spec) = workflow_tool_spec(WORKFLOW_TOOL_NAME) else {
        panic!("Workflow should be a function tool");
    };
    let properties = spec.parameters.properties.unwrap();

    assert!(spec.description.contains("Load the `$workflow` skill"));
    assert!(spec.description.contains("Pass variable runtime data"));
    assert!(spec.description.contains("owning agent"));
    assert!(spec.description.contains("retains orchestration controls"));
    assert!(spec.description.contains("`await listInputs()`"));
    assert!(spec.description.contains("`await readInput(path)`"));
    assert!(spec.description.contains("do not scan the workspace"));
    assert!(
        spec.description
            .contains("distinct from structured agent inputs")
    );
    assert!(
        properties["args"]
            .description
            .as_deref()
            .is_some_and(|description| description.contains("pass structured JSON directly"))
    );
    let output_schema = spec.output_schema.expect("Workflow output schema");
    assert_eq!(
        output_schema["properties"]["transcriptDirKind"]["const"],
        serde_json::json!("appServerHostArtifact")
    );
    assert_eq!(
        output_schema["properties"]["scriptPathKind"]["const"],
        serde_json::json!("appServerHostArtifact")
    );
    assert!(
        output_schema["properties"]["scriptPath"]["description"]
            .as_str()
            .is_some_and(|description| description.contains("not a workspace path"))
    );
    assert!(spec.description.contains("app-server host artifact paths"));
    for text in [
        spec.description.as_str(),
        properties["script"]
            .description
            .as_deref()
            .expect("script description"),
        properties["args"]
            .description
            .as_deref()
            .expect("args description"),
    ] {
        assert!(!text.contains("byte limit"));
        assert!(!text.contains("node limit"));
        assert!(!text.contains("token limit"));
        assert!(!text.contains("prompt limit"));
        assert!(!text.contains("output limit"));
    }
}
