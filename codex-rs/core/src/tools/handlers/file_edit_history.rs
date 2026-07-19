use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::handlers::file_path::is_absolute_file_path;
use crate::tools::handlers::file_path::resolve_tool_path;
use crate::tools::handlers::file_read::FILE_UNCHANGED_STUB;
use crate::tools::handlers::file_read::FileReadArgs;
use crate::tools::handlers::file_read_spec::FILE_READ_TOOL_NAME;
use crate::tools::handlers::file_read_text::EMPTY_FILE_REMINDER;
use crate::tools::handlers::file_state::select_read_content;
use crate::tools::handlers::resolve_tool_environment;
use codex_protocol::models::ResponseItem;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;

pub(super) struct RestoredReadRequest<'a> {
    pub session: &'a Session,
    pub step_context: &'a StepContext,
    pub environment_id: &'a str,
    pub path: &'a PathUri,
    pub current_content: &'a str,
}

pub(super) async fn history_contains_current_read(request: RestoredReadRequest<'_>) -> bool {
    let history = request.session.clone_history().await;
    let mut read_calls = HashMap::new();
    let mut restored_contents = Vec::new();

    for item in history.raw_items() {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } if name == FILE_READ_TOOL_NAME => {
                let Ok(args) = serde_json::from_str::<FileReadArgs>(arguments) else {
                    continue;
                };
                if args.pages.is_none() {
                    read_calls.insert(call_id.clone(), args);
                }
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } if output.success != Some(false) => {
                let Some(args) = read_calls.get(call_id) else {
                    continue;
                };
                if !is_absolute_file_path(&args.file_path) {
                    continue;
                }
                let Some(text) = output.text_content() else {
                    continue;
                };
                if text.starts_with(FILE_UNCHANGED_STUB) {
                    continue;
                }
                let Ok(Some(environment)) = resolve_tool_environment(
                    &request.step_context.environments,
                    args.environment_id.as_deref(),
                ) else {
                    continue;
                };
                if environment.environment_id != request.environment_id {
                    continue;
                }
                let Ok(path) = resolve_tool_path(
                    FILE_READ_TOOL_NAME,
                    "file_path",
                    environment.cwd(),
                    &args.file_path,
                ) else {
                    continue;
                };
                if &path == request.path
                    && let Some(content) = content_from_read_output(text)
                {
                    restored_contents.push((args.offset.unwrap_or(1), args.limit, content));
                }
            }
            _ => {}
        }
    }

    restored_contents
        .into_iter()
        .rev()
        .any(|(offset, limit, content)| {
            content == select_read_content(request.current_content, offset, limit)
        })
}

fn content_from_read_output(output: &str) -> Option<String> {
    if output == EMPTY_FILE_REMINDER {
        return Some(String::new());
    }
    if output.starts_with("<system-reminder>") {
        return None;
    }
    output
        .split('\n')
        .map(|line| {
            let (line_number, content) = line.split_once('\t')?;
            line_number
                .chars()
                .all(|character| character.is_ascii_digit())
                .then_some(content)
        })
        .collect::<Option<Vec<_>>>()
        .map(|lines| lines.join("\n"))
}

#[cfg(test)]
#[path = "file_edit_history_tests.rs"]
mod tests;
