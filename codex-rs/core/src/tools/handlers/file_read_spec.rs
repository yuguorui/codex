use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

use super::file_read_pdf::PDF_MAX_PAGES_PER_READ;

pub(crate) const FILE_READ_TOOL_NAME: &str = "Read";

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FileReadToolOptions {
    pub include_environment_id: bool,
}

pub(crate) fn create_file_read_tool(options: FileReadToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "file_path".to_string(),
            JsonSchema::string(Some("The absolute path to the file to read".to_string())),
        ),
        (
            "offset".to_string(),
            JsonSchema::integer(Some(
                "The line number to start reading from. Only provide if the file is too large to read at once"
                    .to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::integer(Some(
                "The number of lines to read. Only provide if the file is too large to read at once."
                    .to_string(),
            )),
        ),
        (
            "pages".to_string(),
            JsonSchema::string(Some(format!(
                "Page range for PDF files (for example, `1-4` or `3`). Only applicable to PDF files. Maximum {PDF_MAX_PAGES_PER_READ} pages per request."
            ))),
        ),
    ]);
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "The ID of the environment whose filesystem should be read".to_string(),
            )),
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: FILE_READ_TOOL_NAME.to_string(),
        description: "Reads a file from the local filesystem. Text results use line-number and tab prefixes. The file_path must be absolute; offset and limit can select a range from large files. Images are returned visually when the model supports image input. PDF page ranges and Jupyter notebooks, including visual outputs, are returned as multimodal content.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["file_path".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

#[cfg(test)]
#[path = "file_read_spec_tests.rs"]
mod tests;
