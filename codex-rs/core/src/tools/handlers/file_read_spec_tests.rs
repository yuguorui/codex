use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn file_read_spec_matches_claude_code_input_shape() {
    assert_eq!(
        serde_json::to_value(create_file_read_tool(FileReadToolOptions::default()))
            .expect("serialize Read spec"),
        json!({
            "type": "function",
            "name": "Read",
            "description": "Reads a file from the local filesystem. Text results use line-number and tab prefixes. The file_path must be absolute; offset and limit can select a range from large files. Images are returned visually when the model supports image input. PDF page ranges and Jupyter notebooks, including visual outputs, are returned as multimodal content.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to the file to read"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "The number of lines to read. Only provide if the file is too large to read at once."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "The line number to start reading from. Only provide if the file is too large to read at once"
                    },
                    "pages": {
                        "type": "string",
                        "description": "Page range for PDF files (for example, `1-4` or `3`). Only applicable to PDF files. Maximum 4 pages per request."
                    }
                },
                "required": ["file_path"],
                "additionalProperties": false
            }
        })
    );
}

#[test]
fn file_read_spec_can_select_an_environment() {
    let tool = serde_json::to_value(create_file_read_tool(FileReadToolOptions {
        include_environment_id: true,
    }))
    .expect("serialize Read spec");

    assert_eq!(
        tool["parameters"]["properties"]["environment_id"],
        json!({
            "type": "string",
            "description": "The ID of the environment whose filesystem should be read"
        })
    );
}
