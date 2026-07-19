use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::file_change_event::FileChangeEvent;
use crate::tools::handlers::file_change_event::file_change_event;
use crate::tools::handlers::file_edit_content::EditContentRequest;
use crate::tools::handlers::file_edit_content::MAX_EDIT_FILE_SIZE;
use crate::tools::handlers::file_edit_content::prepare_edit;
use crate::tools::handlers::file_edit_history::RestoredReadRequest;
use crate::tools::handlers::file_edit_history::history_contains_current_read;
use crate::tools::handlers::file_edit_spec::FILE_EDIT_TOOL_NAME;
use crate::tools::handlers::file_edit_spec::FileEditToolOptions;
use crate::tools::handlers::file_edit_spec::create_file_edit_tool;
use crate::tools::handlers::file_encoding::DecodedFile;
use crate::tools::handlers::file_encoding::FileEncoding;
use crate::tools::handlers::file_encoding::decode_text_file;
use crate::tools::handlers::file_encoding::encode_file;
use crate::tools::handlers::file_path::bound_function_call_error;
use crate::tools::handlers::file_path::resolve_tool_path;
use crate::tools::handlers::file_path::summarize_tool_argument;
use crate::tools::handlers::file_state::EditStateError;
use crate::tools::handlers::file_state::EditedFileState;
use crate::tools::handlers::file_state::ReadFileState;
use crate::tools::handlers::file_state::normalize_file_content;
use crate::tools::handlers::file_state::session_file_state_cache;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::verified_file_write::ExpectedFileContents;
use crate::tools::handlers::verified_file_write::FileWriteCommitState;
use crate::tools::handlers::verified_file_write::VerifiedFileWrite;
use crate::tools::handlers::verified_file_write::VerifiedFileWriteError;
use crate::tools::handlers::verified_file_write::write_file_verified;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PreToolUsePayload;
use codex_apply_patch::AppliedPatchDelta;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::io;
use std::time::Duration;

pub(crate) struct FileEditHandler {
    options: FileEditToolOptions,
}

impl FileEditHandler {
    pub(crate) fn new(options: FileEditToolOptions) -> Self {
        Self { options }
    }
}

impl Default for FileEditHandler {
    fn default() -> Self {
        Self::new(FileEditToolOptions::default())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEditArgs {
    file_path: String,
    #[serde(default)]
    environment_id: Option<String>,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl ToolExecutor<ToolInvocation> for FileEditHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(FILE_EDIT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_file_edit_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            handle_file_edit(invocation)
                .await
                .map_err(bound_function_call_error)
        })
    }
}

impl CoreToolRuntime for FileEditHandler {
    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        Some(PreToolUsePayload {
            tool_name: HookToolName::new(FILE_EDIT_TOOL_NAME),
            tool_input: serde_json::from_str(arguments).ok()?,
        })
    }
}

async fn handle_file_edit(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        step_context,
        tracker,
        call_id,
        payload,
        ..
    } = invocation;
    let ToolPayload::Function { arguments } = payload else {
        return Err(FunctionCallError::RespondToModel(
            "Edit handler received unsupported payload".to_string(),
        ));
    };
    let args: FileEditArgs = parse_arguments(&arguments)?;
    if args.old_string == args.new_string {
        return Err(FunctionCallError::RespondToModel(
            "No changes to make: old_string and new_string are exactly the same.".to_string(),
        ));
    }

    let Some(turn_environment) =
        resolve_tool_environment(&step_context.environments, args.environment_id.as_deref())?
    else {
        return Err(FunctionCallError::RespondToModel(
            "Edit is unavailable in this session".to_string(),
        ));
    };
    let path = resolve_tool_path(
        FILE_EDIT_TOOL_NAME,
        "file_path",
        turn_environment.cwd(),
        &args.file_path,
    )?;
    let path_summary = summarize_tool_argument(&path.inferred_native_path_string());
    let native_cwd = turn_environment.cwd().to_abs_path().map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "Edit could not resolve the environment working directory: {error}"
        ))
    })?;
    let effective_permissions = apply_granted_turn_permissions(
        session.as_ref(),
        &turn_environment.environment_id,
        native_cwd.as_path(),
        SandboxPermissions::UseDefault,
        /*additional_permissions*/ None,
    )
    .await;
    let sandbox = turn.file_system_sandbox_context(
        effective_permissions.additional_permissions,
        turn_environment,
    );
    let fs = turn_environment.environment.get_filesystem();

    let original = match fs.get_metadata(&path, Some(&sandbox)).await {
        Ok(metadata) => {
            if metadata.size > MAX_EDIT_FILE_SIZE {
                return Err(FunctionCallError::RespondToModel(format!(
                    "File is too large to edit ({} bytes). Maximum editable file size is {MAX_EDIT_FILE_SIZE} bytes.",
                    metadata.size
                )));
            }
            let bytes = fs.read_file(&path, Some(&sandbox)).await.map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "Failed to read file `{path_summary}`: {error}"
                ))
            })?;
            if bytes.len() > MAX_EDIT_FILE_SIZE as usize {
                return Err(FunctionCallError::RespondToModel(format!(
                    "File is too large to edit ({} bytes). Maximum editable file size is {MAX_EDIT_FILE_SIZE} bytes.",
                    bytes.len()
                )));
            }
            let decoded = decode_text_file(bytes, &path_summary, FILE_EDIT_TOOL_NAME)?;
            Some(ExistingFile {
                expected_bytes: encode_file(&decoded.content, decoded.encoding),
                decoded,
                modified_at_ms: metadata.modified_at_ms,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "Failed to inspect file `{path_summary}`: {error}"
            )));
        }
    };

    let file_state_cache = session_file_state_cache(
        &session.services.session_extension_data,
        session.history_version().await,
    );
    if original.is_none() && !args.old_string.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "File does not exist. Current working directory: {}.",
            turn_environment.cwd().inferred_native_path_string()
        )));
    }
    if original.is_some()
        && !args.old_string.is_empty()
        && args
            .file_path
            .trim()
            .to_ascii_lowercase()
            .ends_with(".ipynb")
    {
        return Err(FunctionCallError::RespondToModel(
            "Edit does not support Jupyter notebooks. Modify the notebook with a shell command or a JSON processor such as jq instead."
                .to_string(),
        ));
    }
    if !args.old_string.is_empty()
        && let Some(original) = original.as_ref()
    {
        let current_content = normalize_file_content(&original.decoded.content);
        match file_state_cache.validate_edit(
            &turn_environment.environment_id,
            &path,
            original.modified_at_ms,
            &current_content,
        ) {
            Ok(()) => {}
            Err(EditStateError::NotRead) => {
                if history_contains_current_read(RestoredReadRequest {
                    session: session.as_ref(),
                    step_context: step_context.as_ref(),
                    environment_id: &turn_environment.environment_id,
                    path: &path,
                    current_content: &current_content,
                })
                .await
                {
                    file_state_cache.record_read(
                        &turn_environment.environment_id,
                        &path,
                        ReadFileState {
                            content: current_content,
                            modified_at_ms: original.modified_at_ms,
                            offset: 1,
                            limit: None,
                        },
                    );
                } else {
                    return Err(FunctionCallError::RespondToModel(
                        "File has not been read yet. Read it first before writing to it."
                            .to_string(),
                    ));
                }
            }
            Err(EditStateError::Modified) => {
                return Err(FunctionCallError::RespondToModel(
                    "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it."
                        .to_string(),
                ));
            }
        }
    }

    let original_content = original
        .as_ref()
        .map(|original| original.decoded.content.as_str());
    let edit = prepare_edit(
        EditContentRequest {
            old_string: &args.old_string,
            new_string: &args.new_string,
            replace_all: args.replace_all,
        },
        original_content,
    )?;
    let encoding = original
        .as_ref()
        .map_or(FileEncoding::Utf8, |original| original.decoded.encoding);
    let updated_bytes = encode_file(&edit.updated_content, encoding);
    if updated_bytes.len() > MAX_EDIT_FILE_SIZE as usize {
        return Err(FunctionCallError::RespondToModel(format!(
            "The edited file would exceed the maximum editable file size of {MAX_EDIT_FILE_SIZE} bytes. Use smaller replacements or another editing strategy."
        )));
    }
    let FileChangeEvent { changes, delta } = file_change_event(
        &path,
        original_content,
        &edit.updated_content,
        /*context_radius*/ 3,
    );
    // TODO: Route exact-byte file writes through a shared file-change approval runtime once it
    // can preserve Edit's UTF-16, BOM, CRLF, and missing-final-newline semantics.
    let emitter = ToolEmitter::file_change_for_environment(
        changes,
        /*auto_approved*/ true,
        turn_environment.environment_id.clone(),
    );
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, Some(&tracker));
    emitter.begin(event_ctx).await;

    let expected = original
        .as_ref()
        .map_or(ExpectedFileContents::Missing, |original| {
            ExpectedFileContents::Present(&original.expected_bytes)
        });
    if let Err(error) = write_file_verified(VerifiedFileWrite {
        fs: fs.as_ref(),
        path: &path,
        sandbox: Some(&sandbox),
        expected,
        updated: &updated_bytes,
    })
    .await
    {
        file_state_cache.remove(&turn_environment.environment_id, &path);
        let empty_delta = AppliedPatchDelta::default();
        let applied_delta = match error.commit_state() {
            FileWriteCommitState::NotCommitted => Some(&empty_delta),
            FileWriteCommitState::Committed => delta.as_ref(),
            FileWriteCommitState::Unknown => None,
        };
        let message = edit_write_error_message(error, &path_summary);
        emit_edit_failure(
            &emitter,
            session.as_ref(),
            turn.as_ref(),
            &tracker,
            &call_id,
            &message,
            applied_delta,
        )
        .await;
        return Err(FunctionCallError::RespondToModel(message));
    }
    let modified_at_ms = match fs.get_metadata(&path, Some(&sandbox)).await {
        Ok(metadata) => metadata.modified_at_ms,
        Err(error) => {
            file_state_cache.remove(&turn_environment.environment_id, &path);
            let message =
                format!("Edit could not inspect the written file `{path_summary}`: {error}");
            emit_edit_failure(
                &emitter,
                session.as_ref(),
                turn.as_ref(),
                &tracker,
                &call_id,
                &message,
                delta.as_ref(),
            )
            .await;
            return Err(FunctionCallError::RespondToModel(message));
        }
    };
    file_state_cache.record_edit(
        &turn_environment.environment_id,
        &path,
        EditedFileState {
            content: normalize_file_content(&edit.updated_content),
            modified_at_ms,
        },
    );
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, Some(&tracker));
    emitter
        .finish(
            event_ctx,
            Ok(edit_event_output(
                0,
                "Edit applied successfully".to_string(),
            )),
            delta.as_ref(),
        )
        .await?;

    let file_path = summarize_tool_argument(&args.file_path);
    let message = if args.replace_all {
        format!(
            "The file {file_path} has been updated. All occurrences were successfully replaced."
        )
    } else {
        format!("The file {file_path} has been updated successfully.")
    };
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        message,
        Some(true),
    )))
}

fn edit_write_error_message(error: VerifiedFileWriteError, path_summary: &str) -> String {
    match error {
        VerifiedFileWriteError::ReadBeforeWrite(error) => {
            format!("Edit failed to re-read `{path_summary}` before writing: {error}")
        }
        VerifiedFileWriteError::UnexpectedContents => {
            "File has been unexpectedly modified. Read it again before attempting to write it."
                .to_string()
        }
        VerifiedFileWriteError::CreateParent(error) => {
            format!("Edit failed to create parent directories: {error}")
        }
        VerifiedFileWriteError::Write(error) => {
            format!("Edit failed to write `{path_summary}`: {error}")
        }
        VerifiedFileWriteError::ReadAfterWrite(error) => {
            format!("Edit could not verify the written file `{path_summary}`: {error}")
        }
        VerifiedFileWriteError::UnexpectedWrittenContents => format!(
            "Edit did not produce the expected contents for `{path_summary}`. Read the file again before retrying."
        ),
    }
}

struct ExistingFile {
    decoded: DecodedFile,
    expected_bytes: Vec<u8>,
    modified_at_ms: i64,
}

fn edit_event_output(exit_code: i32, message: String) -> ExecToolCallOutput {
    let (stdout, stderr) = if exit_code == 0 {
        (message, String::new())
    } else {
        (String::new(), message)
    };
    ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(stdout.clone()),
        stderr: StreamOutput::new(stderr.clone()),
        aggregated_output: StreamOutput::new(if exit_code == 0 { stdout } else { stderr }),
        duration: Duration::ZERO,
        timed_out: false,
    }
}

async fn emit_edit_failure(
    emitter: &ToolEmitter,
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    tracker: &crate::tools::context::SharedTurnDiffTracker,
    call_id: &str,
    message: &str,
    applied_patch_delta: Option<&AppliedPatchDelta>,
) {
    let event_ctx = ToolEventCtx::new(session, turn, call_id, Some(tracker));
    let _ = emitter
        .finish(
            event_ctx,
            Ok(edit_event_output(1, message.to_string())),
            applied_patch_delta,
        )
        .await;
}

#[cfg(test)]
#[path = "file_edit_tests.rs"]
mod tests;
