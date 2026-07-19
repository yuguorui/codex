use crate::function_tool::FunctionCallError;
use crate::tools::handlers::file_encoding::decode_text_file;
use crate::tools::handlers::file_encoding::is_utf16_le;
use crate::tools::handlers::file_read_spec::FILE_READ_TOOL_NAME;
use crate::tools::handlers::file_state::normalize_file_content;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_path_uri::PathUri;
use futures::StreamExt;
use std::io;

pub(super) const MAX_EXPLICIT_RANGE_FILE_SIZE: u64 = 512 * 1024 * 1024;
pub(super) const MAX_READ_OUTPUT_TOKENS: usize = 2_000;
pub(super) const EMPTY_FILE_REMINDER: &str =
    "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>";

pub(super) fn read_text_range_with_state(
    text: &str,
    offset: usize,
    limit: Option<usize>,
) -> Result<(String, String), FunctionCallError> {
    if text.is_empty() {
        return Ok((EMPTY_FILE_REMINDER.to_string(), String::new()));
    }
    let lines = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>();
    let total_lines = lines.len();
    let start_index = offset.saturating_sub(1);
    let selected = lines
        .iter()
        .skip(start_index)
        .take(limit.unwrap_or(usize::MAX))
        .copied()
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok((
            format!(
                "<system-reminder>Warning: the file exists but is shorter than the provided offset ({offset}). The file has {total_lines} lines.</system-reminder>"
            ),
            String::new(),
        ));
    }
    let output = selected
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{}\t{line}", offset + index))
        .collect::<Vec<_>>()
        .join("\n");
    validate_read_output_size(&output)?;
    Ok((output, selected.join("\n")))
}

pub(super) async fn read_text_range_stream(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: Option<&FileSystemSandboxContext>,
    path_display: &str,
    offset: usize,
    limit: Option<usize>,
) -> Result<(String, String), FunctionCallError> {
    let mut stream = match fs.read_file_stream(path, sandbox).await {
        Ok(stream) => stream,
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            return read_text_range_buffered(fs, path, sandbox, path_display, offset, limit).await;
        }
        Err(error) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "Failed to read file `{path_display}`: {error}"
            )));
        }
    };
    let mut prefetched_chunks = Vec::new();
    let mut prefix = Vec::with_capacity(2);
    while prefix.len() < 2 {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "Failed to read file `{path_display}`: {error}"
            ))
        })?;
        let remaining = 2usize.saturating_sub(prefix.len());
        prefix.extend(chunk.iter().take(remaining));
        prefetched_chunks.push(chunk);
    }
    if is_utf16_le(&prefix) {
        drop(stream);
        return read_text_range_buffered(fs, path, sandbox, path_display, offset, limit).await;
    }

    let start_index = offset.saturating_sub(1);
    let end_index = start_index.saturating_add(limit.unwrap_or(usize::MAX));
    let mut current_index = 0usize;
    let mut selected_line = Vec::new();
    let mut selected_lines = Vec::new();
    let mut selected_bytes = 0usize;
    let mut total_bytes = 0u64;
    let mut saw_bytes = false;

    let mut prefetched_chunks = prefetched_chunks.into_iter();
    'read_chunks: loop {
        let chunk = match prefetched_chunks.next() {
            Some(chunk) => chunk,
            None => {
                let Some(chunk) = stream.next().await else {
                    break;
                };
                chunk.map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "Failed to read file `{path_display}`: {error}"
                    ))
                })?
            }
        };
        total_bytes = total_bytes
            .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "File size overflowed while reading the requested range".to_string(),
                )
            })?;
        if total_bytes > MAX_EXPLICIT_RANGE_FILE_SIZE {
            return Err(FunctionCallError::RespondToModel(format!(
                "File content exceeds maximum allowed size ({MAX_EXPLICIT_RANGE_FILE_SIZE} bytes)."
            )));
        }
        saw_bytes |= !chunk.is_empty();
        let mut segment_start = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            if (start_index..end_index).contains(&current_index) {
                selected_line.extend_from_slice(&chunk[segment_start..index]);
                push_streamed_line(
                    &mut selected_lines,
                    &mut selected_line,
                    &mut selected_bytes,
                    current_index == 0,
                )?;
            }
            current_index = current_index.saturating_add(1);
            segment_start = index + 1;
            if current_index >= end_index {
                break 'read_chunks;
            }
        }
        if (start_index..end_index).contains(&current_index) {
            selected_line.extend_from_slice(&chunk[segment_start..]);
            if selected_bytes.saturating_add(selected_line.len())
                > MAX_READ_OUTPUT_TOKENS.saturating_mul(4)
            {
                return Err(read_output_too_large_error());
            }
        }
    }

    let total_lines = if saw_bytes {
        if (start_index..end_index).contains(&current_index) {
            push_streamed_line(
                &mut selected_lines,
                &mut selected_line,
                &mut selected_bytes,
                current_index == 0,
            )?;
        }
        current_index.saturating_add(1)
    } else {
        0
    };
    if total_lines == 0 {
        return Ok((EMPTY_FILE_REMINDER.to_string(), String::new()));
    }
    if selected_lines.is_empty() {
        return Ok((
            format!(
                "<system-reminder>Warning: the file exists but is shorter than the provided offset ({offset}). The file has {total_lines} lines.</system-reminder>"
            ),
            String::new(),
        ));
    }
    let state_content = selected_lines.join("\n");
    let output = selected_lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| format!("{}\t{line}", offset + index))
        .collect::<Vec<_>>()
        .join("\n");
    validate_read_output_size(&output)?;
    Ok((output, state_content))
}

async fn read_text_range_buffered(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: Option<&FileSystemSandboxContext>,
    path_display: &str,
    offset: usize,
    limit: Option<usize>,
) -> Result<(String, String), FunctionCallError> {
    let bytes = fs.read_file(path, sandbox).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!("Failed to read file `{path_display}`: {error}"))
    })?;
    validate_actual_read_size(bytes.len(), MAX_EXPLICIT_RANGE_FILE_SIZE)?;
    let decoded = decode_text_file(bytes, path_display, FILE_READ_TOOL_NAME)?;
    let normalized = normalize_file_content(&decoded.content);
    read_text_range_with_state(&normalized, offset, limit)
}

fn push_streamed_line(
    selected_lines: &mut Vec<String>,
    line: &mut Vec<u8>,
    selected_bytes: &mut usize,
    strip_bom: bool,
) -> Result<(), FunctionCallError> {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    let text = String::from_utf8(std::mem::take(line)).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "Read only supports UTF-8 text and supported image files: {error}"
        ))
    })?;
    let text = if strip_bom {
        text.strip_prefix('\u{feff}').unwrap_or(&text).to_string()
    } else {
        text
    };
    *selected_bytes = (*selected_bytes)
        .saturating_add(text.len())
        .saturating_add(32);
    if *selected_bytes > MAX_READ_OUTPUT_TOKENS.saturating_mul(4) {
        return Err(read_output_too_large_error());
    }
    selected_lines.push(text);
    Ok(())
}

pub(super) fn validate_actual_read_size(
    size: usize,
    max_size: u64,
) -> Result<(), FunctionCallError> {
    if u64::try_from(size).unwrap_or(u64::MAX) > max_size {
        return Err(FunctionCallError::RespondToModel(format!(
            "File content ({size} bytes) exceeds maximum allowed size ({max_size} bytes). Use offset and limit parameters to read specific portions of the file."
        )));
    }
    Ok(())
}

fn validate_read_output_size(output: &str) -> Result<(), FunctionCallError> {
    if approx_token_count(output) > MAX_READ_OUTPUT_TOKENS {
        return Err(read_output_too_large_error());
    }
    Ok(())
}

fn read_output_too_large_error() -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "File content exceeds maximum allowed output size ({MAX_READ_OUTPUT_TOKENS} tokens). Use offset and limit parameters to read specific portions of the file."
    ))
}

#[cfg(test)]
#[path = "file_read_text_tests.rs"]
mod tests;
