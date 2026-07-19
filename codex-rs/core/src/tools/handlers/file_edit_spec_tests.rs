use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn file_edit_spec_matches_claude_code_input_shape() {
    assert_eq!(
        serde_json::to_value(create_file_edit_tool(FileEditToolOptions::default()))
            .expect("serialize Edit spec"),
        json!({
            "type": "function",
            "name": "Edit",
            "description": "Performs exact string replacements in files. Use Read on an existing file before editing it. The edit fails when old_string is not unique unless replace_all is true. Use replace_all for replacing or renaming strings throughout a file.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to the file to modify"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The text to replace it with (must be different from old_string)"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The text to replace"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace all occurrences of old_string (default false)"
                    }
                },
                "required": ["file_path", "old_string", "new_string"],
                "additionalProperties": false
            }
        })
    );
}

#[test]
fn file_edit_spec_can_select_an_environment() {
    let tool = serde_json::to_value(create_file_edit_tool(FileEditToolOptions {
        include_environment_id: true,
    }))
    .expect("serialize Edit spec");

    assert_eq!(
        tool["parameters"]["properties"]["environment_id"],
        json!({
            "type": "string",
            "description": "The ID of the environment whose filesystem should be modified"
        })
    );
}
