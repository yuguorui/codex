use crate::function_tool::FunctionCallError;
use crate::tools::handlers::file_read::FILE_READ_IMAGE_RESIZE_LIMITS;
use crate::tools::handlers::file_read::MAX_READ_IMAGES_PER_RESULT;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_utils_image::PromptImageMode;
use codex_utils_image::load_for_prompt_bytes;
use codex_utils_output_truncation::approx_token_count;
use std::path::Path;

const MAX_CELL_OUTPUT_CHARS: usize = 10_000;
const MAX_NOTEBOOK_IMAGE_BYTES: usize = 3_750_000;
const MAX_NOTEBOOK_IMAGES_TOTAL_BYTES: usize = 12_000_000;

pub(super) struct NotebookReadOutput {
    pub content: Vec<FunctionCallOutputContentItem>,
    pub event_summary: String,
}

pub(super) fn render_notebook(
    text: &str,
    path_display: &str,
    max_output_tokens: usize,
) -> Result<NotebookReadOutput, FunctionCallError> {
    let notebook: serde_json::Value = serde_json::from_str(text).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "Failed to parse Jupyter notebook `{path_display}`: {error}"
        ))
    })?;
    let cells = notebook
        .get("cells")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "Failed to parse Jupyter notebook `{path_display}`: missing `cells` array"
            ))
        })?;
    let language = notebook
        .pointer("/metadata/language_info/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("python");
    let mut content = Vec::new();
    let mut text_tokens = 0usize;
    let mut total_image_bytes = 0usize;
    let mut image_count = 0usize;
    for (index, cell) in cells.iter().enumerate() {
        let cell_type = cell
            .get("cell_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let cell_id = cell
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("cell-{index}"));
        let mut metadata = String::new();
        if cell_type != "code" {
            metadata.push_str(&format!("<cell_type>{cell_type}</cell_type>"));
        } else if language != "python" {
            metadata.push_str(&format!("<language>{language}</language>"));
        }
        let source = notebook_text_field(cell.get("source"));
        push_notebook_text(
            &mut content,
            &mut text_tokens,
            format!("<cell id=\"{cell_id}\">{metadata}{source}</cell id=\"{cell_id}\">"),
            max_output_tokens,
        )?;

        if cell_type == "code"
            && let Some(outputs) = cell.get("outputs").and_then(serde_json::Value::as_array)
        {
            let output_text_size = outputs
                .iter()
                .filter_map(notebook_output_text)
                .map(|text| text.chars().count())
                .sum::<usize>();
            let output_text_is_too_large = output_text_size > MAX_CELL_OUTPUT_CHARS;
            if output_text_is_too_large {
                push_notebook_text(
                    &mut content,
                    &mut text_tokens,
                    format!(
                        "\nOutputs are too large to include. Use a shell JSON processor to inspect `.cells[{index}].outputs`."
                    ),
                    max_output_tokens,
                )?;
            }
            for output in outputs {
                if !output_text_is_too_large
                    && let Some(output_text) = notebook_output_text(output)
                    && !output_text.is_empty()
                {
                    push_notebook_text(
                        &mut content,
                        &mut text_tokens,
                        format!("\n{output_text}"),
                        max_output_tokens,
                    )?;
                }
                if let Some((media_type, encoded)) = notebook_output_image(output) {
                    if image_count >= MAX_READ_IMAGES_PER_RESULT {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "Notebook contains more than {MAX_READ_IMAGES_PER_RESULT} image outputs. Inspect specific cells with a shell JSON processor."
                        )));
                    }
                    let encoded = encoded
                        .chars()
                        .filter(|character| !character.is_whitespace());
                    let bytes = BASE64_STANDARD
                        .decode(encoded.collect::<String>())
                        .map_err(|error| {
                            FunctionCallError::RespondToModel(format!(
                                "Failed to decode an image output in notebook `{path_display}`: {error}"
                            ))
                        })?;
                    let extension = if media_type == "image/png" {
                        "png"
                    } else {
                        "jpg"
                    };
                    let image = load_for_prompt_bytes(
                        Path::new(&format!("notebook-output.{extension}")),
                        bytes,
                        PromptImageMode::ResizeWithLimits(FILE_READ_IMAGE_RESIZE_LIMITS),
                    )
                    .map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "Failed to process an image output in notebook `{path_display}`: {error}"
                        ))
                    })?;
                    if image.bytes.len() > MAX_NOTEBOOK_IMAGE_BYTES {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "Notebook image output is too large for the model ({} bytes).",
                            image.bytes.len()
                        )));
                    }
                    total_image_bytes = total_image_bytes.saturating_add(image.bytes.len());
                    if total_image_bytes > MAX_NOTEBOOK_IMAGES_TOTAL_BYTES {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "Notebook image outputs exceed the combined limit of {MAX_NOTEBOOK_IMAGES_TOTAL_BYTES} bytes. Inspect specific cells with a shell JSON processor."
                        )));
                    }
                    image_count = image_count.saturating_add(1);
                    content.push(FunctionCallOutputContentItem::InputImage {
                        image_url: image.into_data_url(),
                        detail: Some(DEFAULT_IMAGE_DETAIL),
                    });
                }
            }
        }
    }
    if content.is_empty() {
        content.push(FunctionCallOutputContentItem::InputText {
            text: "The notebook contains no cells.".to_string(),
        });
    }
    Ok(NotebookReadOutput {
        content,
        event_summary: format!("Read notebook `{path_display}` ({} cells)", cells.len()),
    })
}

fn push_notebook_text(
    content: &mut Vec<FunctionCallOutputContentItem>,
    text_tokens: &mut usize,
    text: String,
    max_output_tokens: usize,
) -> Result<(), FunctionCallError> {
    *text_tokens = text_tokens.saturating_add(approx_token_count(&text));
    if *text_tokens > max_output_tokens {
        return Err(FunctionCallError::RespondToModel(format!(
            "File content exceeds maximum allowed output size ({max_output_tokens} tokens). Use a shell JSON processor to read specific notebook cells."
        )));
    }
    if let Some(FunctionCallOutputContentItem::InputText { text: previous }) = content.last_mut() {
        previous.push('\n');
        previous.push_str(&text);
    } else {
        content.push(FunctionCallOutputContentItem::InputText { text });
    }
    Ok(())
}

fn notebook_output_text(output: &serde_json::Value) -> Option<String> {
    let output_type = output
        .get("output_type")
        .and_then(serde_json::Value::as_str);
    if output_type == Some("error") {
        let name = output
            .get("ename")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let value = output
            .get("evalue")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let traceback = notebook_text_field(output.get("traceback"));
        return Some(format!("{name}: {value}\n{traceback}"));
    }
    if let Some(text) = output.get("text") {
        return Some(notebook_text_field(Some(text)));
    }
    output
        .pointer("/data/text/plain")
        .map(|text| notebook_text_field(Some(text)))
}

fn notebook_output_image(output: &serde_json::Value) -> Option<(&'static str, &str)> {
    let data = output.get("data")?;
    for (key, media_type) in [("image/png", "image/png"), ("image/jpeg", "image/jpeg")] {
        if let Some(encoded) = data.get(key).and_then(serde_json::Value::as_str) {
            return Some((media_type, encoded));
        }
    }
    None
}

fn notebook_text_field(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(lines)) => lines
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<String>(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
#[path = "file_read_notebook_tests.rs"]
mod tests;
