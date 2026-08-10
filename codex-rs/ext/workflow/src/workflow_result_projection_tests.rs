use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;

#[test]
fn json_pointer_selects_nested_values_and_escapes_tokens() {
    let serialized = r#"{"reports":[{"files":{"a/b~":"chosen"}}],"other":"value"}"#;

    let projected = project_workflow_result(serialized, "/reports/0/files/a~1b~0").unwrap();

    assert_eq!(projected.value, json!("chosen"));
    assert_eq!(projected.serialized, r#""chosen""#);
    assert_eq!(
        projected.sha256,
        format!("{:x}", Sha256::digest(r#""chosen""#.as_bytes()))
    );
}

#[test]
fn a_long_valid_pointer_still_produces_bounded_projection_metadata() {
    let key = "k".repeat(511);
    let source = serde_json::from_str::<JsonValue>(&format!(r#"{{"{key}":"chosen"}}"#)).unwrap();
    let projected = project_workflow_result(&source.to_string(), &format!("/{key}")).unwrap();

    assert_eq!(projected.json_pointer.len(), 512);
    assert_eq!(projected.value, json!("chosen"));
}

#[test]
fn empty_json_pointer_selects_the_complete_value() {
    let projected = project_workflow_result(r#"{"answer":42}"#, "").unwrap();

    assert_eq!(projected.value, json!({"answer": 42}));
    assert_eq!(projected.serialized, r#"{"answer":42}"#);
}

#[test]
fn invalid_and_missing_json_pointers_fail_with_actionable_errors() {
    let cases = [
        ("reports", "jsonPointer must be empty or start with '/'"),
        (
            "/a~2",
            "jsonPointer escape sequences must use ~0 or ~1 instead of a raw '~'",
        ),
        ("/a~", "jsonPointer ends with an incomplete ~0 or ~1 escape"),
    ];
    for (json_pointer, expected) in cases {
        assert_eq!(
            project_workflow_result(r#"{"a":1}"#, json_pointer).unwrap_err(),
            expected.to_string()
        );
    }
    assert_eq!(
        project_workflow_result(r#"{"a":1}"#, "/missing").unwrap_err(),
        r#"jsonPointer "/missing" does not select a value in the workflow result"#.to_string()
    );
    assert_eq!(
        project_workflow_result(r#"{"a":1}"#, &format!("/{}", "a".repeat(513))).unwrap_err(),
        "choose a jsonPointer no longer than 512 UTF-8 bytes".to_string()
    );
}
