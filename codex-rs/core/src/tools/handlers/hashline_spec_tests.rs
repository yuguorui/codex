use super::*;

#[test]
fn schema_includes_environment_id_when_requested() {
    let tool = create_hashline_tool(HashlineToolOptions {
        include_environment_id: true,
    });

    let ToolSpec::Function(function) = tool else {
        panic!("expected function tool");
    };
    let properties = function
        .parameters
        .properties
        .expect("object schema should have properties");

    assert_eq!(function.name, "fuzz_view_edit");
    assert!(properties.contains_key("environment_id"));
    assert!(properties.contains_key("anchor"));
    assert!(properties.contains_key("content"));
}
