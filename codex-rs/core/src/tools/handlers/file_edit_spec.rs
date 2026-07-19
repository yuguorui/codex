use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) const FILE_EDIT_TOOL_NAME: &str = "Edit";

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FileEditToolOptions {
    pub include_environment_id: bool,
}

pub(crate) fn create_file_edit_tool(options: FileEditToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "file_path".to_string(),
            JsonSchema::string(Some("The absolute path to the file to modify".to_string())),
        ),
        (
            "old_string".to_string(),
            JsonSchema::string(Some("The text to replace".to_string())),
        ),
        (
            "new_string".to_string(),
            JsonSchema::string(Some(
                "The text to replace it with (must be different from old_string)".to_string(),
            )),
        ),
        (
            "replace_all".to_string(),
            JsonSchema::boolean(Some(
                "Replace all occurrences of old_string (default false)".to_string(),
            )),
        ),
    ]);
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "The ID of the environment whose filesystem should be modified".to_string(),
            )),
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: FILE_EDIT_TOOL_NAME.to_string(),
        description: "Performs exact string replacements in files. Use Read on an existing file before editing it. The edit fails when old_string is not unique unless replace_all is true. Use replace_all for replacing or renaming strings throughout a file.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec![
                "file_path".to_string(),
                "old_string".to_string(),
                "new_string".to_string(),
            ]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

#[cfg(test)]
#[path = "file_edit_spec_tests.rs"]
mod tests;
