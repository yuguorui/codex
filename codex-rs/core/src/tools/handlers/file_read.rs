use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::file_encoding::decode_text_file;
use crate::tools::handlers::file_path::blocked_binary_extension;
use crate::tools::handlers::file_path::bound_function_call_error;
use crate::tools::handlers::file_path::is_blocked_device_path;
use crate::tools::handlers::file_path::resolve_tool_path;
use crate::tools::handlers::file_path::summarize_tool_argument;
use crate::tools::handlers::file_read_notebook::render_notebook;
use crate::tools::handlers::file_read_pdf::PDF_MAX_EXTRACT_SIZE;
use crate::tools::handlers::file_read_pdf::parse_pdf_page_range;
use crate::tools::handlers::file_read_pdf::render_pdf_pages;
use crate::tools::handlers::file_read_spec::FILE_READ_TOOL_NAME;
use crate::tools::handlers::file_read_spec::FileReadToolOptions;
use crate::tools::handlers::file_read_spec::create_file_read_tool;
use crate::tools::handlers::file_read_text::MAX_EXPLICIT_RANGE_FILE_SIZE;
use crate::tools::handlers::file_read_text::MAX_READ_OUTPUT_TOKENS;
use crate::tools::handlers::file_read_text::read_text_range_stream;
use crate::tools::handlers::file_read_text::read_text_range_with_state;
use crate::tools::handlers::file_read_text::validate_actual_read_size;
use crate::tools::handlers::file_state::ReadFileState;
use crate::tools::handlers::file_state::normalize_file_content;
use crate::tools::handlers::file_state::session_file_state_cache;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PreToolUsePayload;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::ExecCommandSource;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_image::PromptImageMode;
use codex_utils_image::PromptImageResizeLimits;
use codex_utils_image::load_for_prompt_bytes;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use std::io;
use std::path::Path;
use std::time::Duration;

const DEFAULT_MAX_FILE_SIZE: u64 = 256 * 1024;
const MAX_READ_IMAGE_BYTES: u64 = 3_750_000;
pub(super) const MAX_READ_IMAGES_PER_RESULT: usize = 4;
pub(super) const FILE_READ_IMAGE_RESIZE_LIMITS: PromptImageResizeLimits = PromptImageResizeLimits {
    max_dimension: 1024,
    max_patches: 1_600,
};
pub(super) const FILE_UNCHANGED_STUB: &str = "File unchanged since last read. The content from the earlier Read tool_result in this conversation is still current \u{2014} refer to that instead of re-reading.";

pub(crate) struct FileReadHandler {
    options: FileReadToolOptions,
}

impl FileReadHandler {
    pub(crate) fn new(options: FileReadToolOptions) -> Self {
        Self { options }
    }
}

impl Default for FileReadHandler {
    fn default() -> Self {
        Self::new(FileReadToolOptions::default())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileReadArgs {
    pub file_path: String,
    #[serde(default)]
    pub environment_id: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    pub pages: Option<String>,
}

impl ToolExecutor<ToolInvocation> for FileReadHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(FILE_READ_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_file_read_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            handle_file_read(invocation)
                .await
                .map_err(bound_function_call_error)
        })
    }
}

impl CoreToolRuntime for FileReadHandler {
    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        Some(PreToolUsePayload {
            tool_name: HookToolName::new(FILE_READ_TOOL_NAME),
            tool_input: serde_json::from_str(arguments).ok()?,
        })
    }
}

async fn handle_file_read(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        step_context,
        call_id,
        payload,
        ..
    } = invocation;
    let ToolPayload::Function { arguments } = payload else {
        return Err(FunctionCallError::RespondToModel(
            "Read handler received unsupported payload".to_string(),
        ));
    };
    let args: FileReadArgs = parse_arguments(&arguments)?;
    let offset = args.offset.unwrap_or(1);
    if args.limit == Some(0) {
        return Err(FunctionCallError::RespondToModel(
            "Read limit must be greater than zero".to_string(),
        ));
    }
    let pdf_pages = args
        .pages
        .as_deref()
        .map(parse_pdf_page_range)
        .transpose()?;
    let Some(turn_environment) =
        resolve_tool_environment(&step_context.environments, args.environment_id.as_deref())?
    else {
        return Err(FunctionCallError::RespondToModel(
            "Read is unavailable in this session".to_string(),
        ));
    };
    let path = resolve_tool_path(
        FILE_READ_TOOL_NAME,
        "file_path",
        turn_environment.cwd(),
        &args.file_path,
    )?;
    let path_display = path.inferred_native_path_string();
    let path_summary = summarize_tool_argument(&path_display);
    if is_blocked_device_path(&path_display) {
        return Err(FunctionCallError::RespondToModel(format!(
            "Cannot read '{}': this device file would block or produce infinite output.",
            summarize_tool_argument(&args.file_path)
        )));
    }
    if let Some(extension) = blocked_binary_extension(&path_display) {
        return Err(FunctionCallError::RespondToModel(format!(
            "This tool cannot read binary files. The file appears to be a binary {extension} file. Please use appropriate tools for binary file analysis."
        )));
    }
    let native_cwd = turn_environment.cwd().to_abs_path().map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "Read could not resolve the environment working directory: {error}"
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
    let metadata = fs
        .get_metadata(&path, Some(&sandbox))
        .await
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FunctionCallError::RespondToModel(format!(
                "File does not exist. Current working directory: {}.",
                turn_environment.cwd().inferred_native_path_string()
            )),
            _ => FunctionCallError::RespondToModel(format!(
                "Failed to inspect file `{path_summary}`: {error}"
            )),
        })?;
    if !metadata.is_file {
        return Err(FunctionCallError::RespondToModel(format!(
            "Read only supports files; `{path_summary}` is not a file."
        )));
    }
    let is_image = is_image_path(&path_display);
    let is_pdf = path_display.to_ascii_lowercase().ends_with(".pdf");
    let is_notebook = path_display.to_ascii_lowercase().ends_with(".ipynb");
    if is_notebook && metadata.size > DEFAULT_MAX_FILE_SIZE {
        return Err(FunctionCallError::RespondToModel(format!(
            "Notebook content ({} bytes) exceeds maximum allowed size ({DEFAULT_MAX_FILE_SIZE} bytes). Use a shell JSON processor to read specific notebook cells.",
            metadata.size
        )));
    }
    if is_image {
        validate_image_prompt_size(metadata.size, "Image")?;
    }
    if is_pdf && metadata.size > PDF_MAX_EXTRACT_SIZE {
        return Err(FunctionCallError::RespondToModel(format!(
            "PDF file exceeds maximum allowed size for page extraction ({PDF_MAX_EXTRACT_SIZE} bytes)."
        )));
    }
    let max_size = args
        .limit
        .map_or(DEFAULT_MAX_FILE_SIZE, |_| MAX_EXPLICIT_RANGE_FILE_SIZE);
    if !is_image && !is_pdf && metadata.size > max_size {
        return Err(FunctionCallError::RespondToModel(format!(
            "File content ({} bytes) exceeds maximum allowed size ({max_size} bytes). Use offset and limit parameters to read specific portions of the file.",
            metadata.size
        )));
    }
    let emitter = ToolEmitter::read(
        vec![FILE_READ_TOOL_NAME.to_string(), path_display.clone()],
        turn_environment.cwd().clone(),
        ExecCommandSource::Agent,
        path.basename().unwrap_or_else(|| path_display.clone()),
        path.to_path_buf(),
    );
    let file_state_cache = session_file_state_cache(
        &session.services.session_extension_data,
        session.history_version().await,
    );
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
    emitter.begin(event_ctx).await;
    let read_result: Result<(FunctionToolOutput, String, Option<String>), FunctionCallError> =
        async {
        if is_image {
            if !turn
                .model_info
                .input_modalities
                .contains(&InputModality::Image)
            {
                return Err(FunctionCallError::RespondToModel(
                    "Read cannot return an image because this model does not support image inputs"
                        .to_string(),
                ));
            }
            let bytes = read_file_bytes(fs.as_ref(), &path, &sandbox, &path_summary).await?;
            validate_image_prompt_size(
                u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                "Image",
            )?;
            let image = load_for_prompt_bytes(
                Path::new(&path_display),
                bytes,
                PromptImageMode::ResizeWithLimits(FILE_READ_IMAGE_RESIZE_LIMITS),
            )
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "Failed to decode image `{path_summary}`: {error}"
                ))
            })?;
            validate_image_prompt_size(
                u64::try_from(image.bytes.len()).unwrap_or(u64::MAX),
                "Processed image",
            )?;
            return Ok((
                FunctionToolOutput::from_content(
                    vec![FunctionCallOutputContentItem::InputImage {
                        image_url: image.into_data_url(),
                        detail: Some(DEFAULT_IMAGE_DETAIL),
                    }],
                    Some(true),
                ),
                format!("Read image `{path_summary}` ({} bytes)", metadata.size),
                None,
            ));
        }

        if is_pdf {
            if !turn
                .model_info
                .input_modalities
                .contains(&InputModality::Image)
            {
                return Err(FunctionCallError::RespondToModel(
                    "Read cannot return PDF pages because this model does not support image inputs"
                        .to_string(),
                ));
            }
            let bytes = read_file_bytes(fs.as_ref(), &path, &sandbox, &path_summary).await?;
            validate_actual_read_size(bytes.len(), PDF_MAX_EXTRACT_SIZE)?;
            let pdf = render_pdf_pages(bytes, &path_summary, pdf_pages).await?;
            return Ok((
                FunctionToolOutput::from_content(pdf.content, Some(true)),
                pdf.event_summary,
                None,
            ));
        }

        if is_notebook {
            let bytes = read_file_bytes(fs.as_ref(), &path, &sandbox, &path_summary).await?;
            validate_actual_read_size(bytes.len(), DEFAULT_MAX_FILE_SIZE)?;
            let text = String::from_utf8(bytes).map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "Read only supports UTF-8 Jupyter notebooks; `{path_summary}` is not valid UTF-8: {error}"
                ))
            })?;
            let normalized = normalize_file_content(&text);
            let result = render_notebook(&normalized, &path_summary, MAX_READ_OUTPUT_TOKENS)?;
            if !turn
                .model_info
                .input_modalities
                .contains(&InputModality::Image)
                && result
                    .content
                    .iter()
                    .any(|item| matches!(item, FunctionCallOutputContentItem::InputImage { .. }))
            {
                return Err(FunctionCallError::RespondToModel(
                    "Read cannot return notebook images because this model does not support image inputs"
                        .to_string(),
                ));
            }
            return Ok((
                FunctionToolOutput::from_content(result.content, Some(true)),
                result.event_summary,
                None,
            ));
        }

        let (result, state_content) = if args.limit.is_some() {
            read_text_range_stream(
                fs.as_ref(),
                &path,
                Some(&sandbox),
                &path_summary,
                offset,
                args.limit,
            )
            .await?
        } else {
            let bytes = read_file_bytes(fs.as_ref(), &path, &sandbox, &path_summary).await?;
            validate_actual_read_size(bytes.len(), DEFAULT_MAX_FILE_SIZE)?;
            let decoded = decode_text_file(bytes, &path_summary, FILE_READ_TOOL_NAME)?;
            let normalized = normalize_file_content(&decoded.content);
            read_text_range_with_state(&normalized, offset, None)?
        };
        Ok((
            FunctionToolOutput::from_text(result.clone(), Some(true)),
            result,
            Some(state_content),
        ))
    }
    .await;

    let (mut output, mut event_stdout, state_content) = match read_result {
        Ok(output) => output,
        Err(error) => {
            let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
            let _ = emitter
                .finish(
                    event_ctx,
                    Ok(read_event_error_output(error.to_string())),
                    None,
                )
                .await;
            return Err(error);
        }
    };
    if let Some(content) = state_content {
        let history_policy: codex_utils_output_truncation::TruncationPolicy =
            turn.model_info.truncation_policy.into();
        let output_will_be_retained = event_stdout.len() <= (history_policy * 1.2).byte_budget();
        if output_will_be_retained
            && file_state_cache.read_is_unchanged(
                &turn_environment.environment_id,
                &path,
                metadata.modified_at_ms,
                offset,
                args.limit,
                &content,
            )
        {
            output = FunctionToolOutput::from_text(FILE_UNCHANGED_STUB.to_string(), Some(true));
            event_stdout = FILE_UNCHANGED_STUB.to_string();
        } else if output_will_be_retained {
            file_state_cache.record_read(
                &turn_environment.environment_id,
                &path,
                ReadFileState {
                    content,
                    modified_at_ms: metadata.modified_at_ms,
                    offset,
                    limit: args.limit,
                },
            );
        } else {
            file_state_cache.remove(&turn_environment.environment_id, &path);
        }
    }
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
    emitter
        .finish(event_ctx, Ok(read_event_output(event_stdout)), None)
        .await?;
    Ok(boxed_tool_output(output))
}

fn is_image_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

fn validate_image_prompt_size(size: u64, label: &str) -> Result<(), FunctionCallError> {
    if size > MAX_READ_IMAGE_BYTES {
        return Err(FunctionCallError::RespondToModel(format!(
            "{label} content ({size} bytes) exceeds the maximum safe prompt size ({MAX_READ_IMAGE_BYTES} bytes). Resize or compress the image before reading it."
        )));
    }
    Ok(())
}

fn read_event_output(stdout: String) -> ExecToolCallOutput {
    ExecToolCallOutput {
        exit_code: 0,
        stdout: StreamOutput::new(stdout.clone()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(stdout),
        duration: Duration::ZERO,
        timed_out: false,
    }
}

fn read_event_error_output(stderr: String) -> ExecToolCallOutput {
    ExecToolCallOutput {
        exit_code: 1,
        stdout: StreamOutput::new(String::new()),
        stderr: StreamOutput::new(stderr.clone()),
        aggregated_output: StreamOutput::new(stderr),
        duration: Duration::ZERO,
        timed_out: false,
    }
}

async fn read_file_bytes(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: &FileSystemSandboxContext,
    path_display: &str,
) -> Result<Vec<u8>, FunctionCallError> {
    fs.read_file(path, Some(sandbox)).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!("Failed to read file `{path_display}`: {error}"))
    })
}

#[cfg(test)]
#[path = "file_read_tests.rs"]
mod tests;
