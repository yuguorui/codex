use codex_exec_server::FileSystemSandboxContext;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::FileChange;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use similar::TextDiff;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::hashline_spec::HashlineToolOptions;
use crate::tools::handlers::hashline_spec::create_hashline_tool;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PreToolUsePayload;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

const DEFAULT_CONTEXT_LINES: usize = 2;
const FUZZY_RELOCATE_RADIUS: usize = 3;
const MAX_READ_LINES: usize = 500;
const MAX_FORMATTED_LINE_CHARS: usize = 4000;

pub(crate) struct HashlineHandler {
    options: HashlineToolOptions,
}

impl HashlineHandler {
    pub(crate) fn new(options: HashlineToolOptions) -> Self {
        Self { options }
    }
}

impl Default for HashlineHandler {
    fn default() -> Self {
        Self {
            options: HashlineToolOptions {
                include_environment_id: false,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct HashlineArgs {
    action: HashlineAction,
    path: String,
    #[serde(default)]
    environment_id: Option<String>,
    #[serde(default)]
    anchor: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    before: bool,
    #[serde(default)]
    context: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HashlineAction {
    Read,
    Edit,
    Insert,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Document {
    lines: Vec<String>,
    newline: &'static str,
    trailing_newline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anchor {
    Hash(LineHash),
    LineHash { line: usize, hash: LineHash },
    Line(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolvedAnchor {
    index: usize,
    hash: LineHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolveMode {
    Strict,
    Fuzzy,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum LineHash {
    Short(u8),
    Full(u16),
}

impl ToolExecutor<ToolInvocation> for HashlineHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("fuzz_view_edit")
    }

    fn spec(&self) -> ToolSpec {
        create_hashline_tool(self.options)
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl HashlineHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "fuzz_view_edit handler received unsupported payload".to_string(),
            ));
        };
        let args: HashlineArgs = parse_arguments(&arguments)?;
        let Some(turn_environment) =
            resolve_tool_environment(&turn.environments, args.environment_id.as_deref())?
        else {
            return Err(FunctionCallError::RespondToModel(
                "fuzz_view_edit is unavailable in this session".to_string(),
            ));
        };
        let cwd = turn_environment.cwd();
        let path = resolve_hashline_path(cwd, &args.path)?;
        let sandbox = turn.file_system_sandbox_context(
            /*additional_permissions*/ None,
            /*environment*/ turn_environment,
        );

        let output = match args.action {
            HashlineAction::Read => {
                let emitter = hashline_read_emitter(args.action, &path, cwd);
                let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
                emitter.begin(event_ctx).await;
                let result = match read_document(turn_environment, &path, Some(&sandbox)).await {
                    Ok(doc) => {
                        read_output(&doc, args.anchor.as_deref(), context_lines(args.context))
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(output) => {
                        let event_ctx =
                            ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
                        emitter
                            .finish(
                                event_ctx,
                                Ok(hashline_output(0, output.clone(), String::new())),
                                None,
                            )
                            .await?;
                        output
                    }
                    Err(error) => {
                        let event_ctx =
                            ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
                        let _ = emitter
                            .finish(
                                event_ctx,
                                Ok(hashline_output(1, String::new(), error.to_string())),
                                None,
                            )
                            .await;
                        return Err(error);
                    }
                }
            }
            HashlineAction::Edit | HashlineAction::Insert | HashlineAction::Delete => {
                let mut doc = read_document(turn_environment, &path, Some(&sandbox)).await?;
                let before = doc.render();
                let content = args.content.as_deref();
                let changed = apply_mutation(&mut doc, &args, content)?;
                let after = doc.render();
                if before == after {
                    mutation_output(changed, &doc, context_lines(args.context), None)
                } else {
                    let unified_diff = hashline_unified_diff(&before, &after);
                    let changes = HashMap::from([(
                        path_event_key(&path),
                        FileChange::Update {
                            unified_diff,
                            move_path: None,
                        },
                    )]);
                    let emitter = ToolEmitter::apply_patch_for_environment(
                        changes,
                        /*auto_approved*/ true,
                        turn_environment.environment_id.clone(),
                    );
                    let event_ctx = ToolEventCtx::new(
                        session.as_ref(),
                        turn.as_ref(),
                        &call_id,
                        Some(&tracker),
                    );
                    emitter.begin(event_ctx).await;
                    let fs = turn_environment.environment.get_filesystem();
                    if let Err(error) = fs
                        .write_file(&path, after.into_bytes(), Some(&sandbox))
                        .await
                    {
                        let message = format!(
                            "fuzz_view_edit failed to write `{}`: {error}",
                            path_display(&path)
                        );
                        let event_ctx = ToolEventCtx::new(
                            session.as_ref(),
                            turn.as_ref(),
                            &call_id,
                            Some(&tracker),
                        );
                        emitter
                            .finish(
                                event_ctx,
                                Ok(hashline_output(1, String::new(), message.clone())),
                                None,
                            )
                            .await?;
                        return Err(FunctionCallError::RespondToModel(message));
                    }
                    let event_ctx = ToolEventCtx::new(
                        session.as_ref(),
                        turn.as_ref(),
                        &call_id,
                        Some(&tracker),
                    );
                    emitter
                        .finish(
                            event_ctx,
                            Ok(hashline_output(0, String::new(), String::new())),
                            None,
                        )
                        .await?;
                    mutation_output(changed, &doc, context_lines(args.context), Some(&path))
                }
            }
        };

        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            output,
            Some(true),
        )))
    }
}

impl CoreToolRuntime for HashlineHandler {
    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        let input = serde_json::from_str(arguments).ok()?;
        Some(PreToolUsePayload {
            tool_name: HookToolName::new("fuzz_view_edit"),
            tool_input: input,
        })
    }
}

fn hashline_output(exit_code: i32, stdout: String, stderr: String) -> ExecToolCallOutput {
    ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(stdout.clone()),
        stderr: StreamOutput::new(stderr.clone()),
        aggregated_output: StreamOutput::new(if exit_code != 0 { stderr } else { stdout }),
        duration: Duration::ZERO,
        timed_out: false,
    }
}
fn hashline_read_emitter(action: HashlineAction, path: &PathUri, cwd: &PathUri) -> ToolEmitter {
    let action_name_str = action_name(action);
    let path_display = path_display(path);
    let file_name = path.basename().unwrap_or_else(|| path_display.clone());
    ToolEmitter::read(
        vec![
            "fuzz_view_edit".to_string(),
            action_name_str.to_string(),
            path_display,
        ],
        cwd.clone(),
        ExecCommandSource::Agent,
        file_name,
        path_event_key(path),
    )
}

async fn read_document(
    turn_environment: &TurnEnvironment,
    path: &PathUri,
    sandbox: Option<&FileSystemSandboxContext>,
) -> Result<Document, FunctionCallError> {
    let fs = turn_environment.environment.get_filesystem();
    let bytes = fs.read_file(path, sandbox).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "fuzz_view_edit failed to read `{}`: {error}",
            path_display(path)
        ))
    })?;
    let text = String::from_utf8(bytes).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "fuzz_view_edit only supports UTF-8 text files; `{}` is not valid UTF-8: {error}",
            path_display(path)
        ))
    })?;
    Ok(Document::parse(&text))
}

fn resolve_hashline_path(cwd: &PathUri, path: &str) -> Result<PathUri, FunctionCallError> {
    if path.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "fuzz_view_edit requires a non-empty `path`".to_string(),
        ));
    }
    if let Ok(uri) = PathUri::parse(path) {
        return Ok(uri);
    }

    let legacy_path: LegacyAppPathString =
        serde_json::from_value(serde_json::Value::String(path.to_string())).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "fuzz_view_edit path `{path}` is not valid UTF-8 path text: {error}"
            ))
        })?;
    if let Some(convention) = legacy_path.infer_absolute_path_convention() {
        return legacy_path.to_path_uri(convention).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "fuzz_view_edit path `{path}` is not a valid absolute {convention} path: {error}"
            ))
        });
    }

    cwd.join(&path.replace('\\', "/")).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "fuzz_view_edit failed to resolve `{path}` against `{cwd}`: {error}"
        ))
    })
}

fn path_display(path: &PathUri) -> String {
    path.inferred_native_path_string()
}

fn path_event_key(path: &PathUri) -> PathBuf {
    path.to_abs_path()
        .map(AbsolutePathBuf::into_path_buf)
        .unwrap_or_else(|_| PathBuf::from(path_display(path)))
}

fn require_anchor(args: &HashlineArgs) -> Result<&str, FunctionCallError> {
    args.anchor.as_deref().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "fuzz_view_edit action `{}` requires `anchor`",
            action_name(args.action)
        ))
    })
}

fn require_content(args: &HashlineArgs) -> Result<&str, FunctionCallError> {
    args.content.as_deref().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "fuzz_view_edit action `{}` requires `content`",
            action_name(args.action)
        ))
    })
}

fn context_lines(context: Option<usize>) -> usize {
    context.unwrap_or(DEFAULT_CONTEXT_LINES).min(20)
}

fn action_name(action: HashlineAction) -> &'static str {
    match action {
        HashlineAction::Read => "read",
        HashlineAction::Edit => "edit",
        HashlineAction::Insert => "insert",
        HashlineAction::Delete => "delete",
    }
}

impl Document {
    fn parse(text: &str) -> Self {
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let trailing_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let body = body.strip_suffix('\r').unwrap_or(body);
        let lines = if body.is_empty() {
            Vec::new()
        } else if newline == "\r\n" {
            body.split("\r\n").map(ToString::to_string).collect()
        } else {
            body.split('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
                .collect()
        };
        Self {
            lines,
            newline,
            trailing_newline,
        }
    }

    fn render(&self) -> String {
        let mut rendered = self.lines.join(self.newline);
        if self.trailing_newline {
            rendered.push_str(self.newline);
        }
        rendered
    }

    fn hash_at(&self, index: usize) -> LineHash {
        full_hash(&self.lines[index])
    }

    fn hash_index(&self) -> HashMap<LineHash, Vec<usize>> {
        let mut index = HashMap::new();
        for (line_index, line) in self.lines.iter().enumerate() {
            let full = full_hash(line);
            index.entry(full).or_insert_with(Vec::new).push(line_index);
            index
                .entry(full.short())
                .or_insert_with(Vec::new)
                .push(line_index);
        }
        index
    }
}

fn read_output(
    doc: &Document,
    anchor: Option<&str>,
    context: usize,
) -> Result<String, FunctionCallError> {
    let mut notes = Vec::new();
    let (start, end) = if let Some(anchor) = anchor {
        if let Some((left, right)) = anchor.split_once("..") {
            if right.contains("..") {
                return Err(invalid_anchor(anchor));
            }
            let start_index = if left.is_empty() {
                0
            } else {
                read_anchor_index(doc, left)?
            };
            let end_index = if right.is_empty() {
                (start_index + MAX_READ_LINES).min(doc.lines.len())
            } else {
                read_anchor_index(doc, right)?.saturating_add(1)
            };
            let requested_end_line = read_requested_end_line(right, end_index);
            if start_index >= doc.lines.len() {
                return Ok(format!(
                    "(no lines: requested range starts past end of file; file has {} lines)",
                    doc.lines.len()
                ));
            }
            let clamped_end_index = end_index.min(doc.lines.len());
            if start_index > end_index {
                return Err(FunctionCallError::RespondToModel(format!(
                    "fuzz_view_edit range `{anchor}` resolves backwards"
                )));
            }
            if end_index > clamped_end_index {
                notes.push(format!(
                    "(showing through end of file; requested range extends past line {} but file has {} lines)",
                    requested_end_line,
                    doc.lines.len()
                ));
            }
            (start_index, clamped_end_index)
        } else {
            let parsed = parse_anchor(anchor, /*allow_bare_line*/ true)?;
            if let Anchor::Line(line) = parsed
                && line > doc.lines.len()
            {
                return Ok(format!(
                    "(no lines: requested line {line} is past end of file; file has {} lines)",
                    doc.lines.len()
                ));
            }
            let resolved = resolve_anchor(doc, parsed, ResolveMode::Fuzzy)?;
            context_window(doc.lines.len(), resolved.index, resolved.index, context)
        }
    } else {
        let end = doc.lines.len().min(MAX_READ_LINES);
        (0, end)
    };
    let mut output = format_lines(doc, start, end, None);
    if anchor.is_none() && doc.lines.len() > MAX_READ_LINES {
        output.push_str(&format!(
            "\n(showing first {MAX_READ_LINES} of {} lines; use anchor with line range to read more)",
            doc.lines.len()
        ));
    }
    for note in notes {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&note);
    }
    Ok(output)
}

fn read_anchor_index(doc: &Document, anchor: &str) -> Result<usize, FunctionCallError> {
    match parse_anchor(anchor, /*allow_bare_line*/ true)? {
        Anchor::Line(line) => Ok((line - 1).min(doc.lines.len())),
        anchor => Ok(resolve_anchor(doc, anchor, ResolveMode::Fuzzy)?.index),
    }
}

fn read_requested_end_line(right_anchor: &str, end_index: usize) -> usize {
    right_anchor
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|line| *line > 0)
        .unwrap_or(end_index)
}

fn apply_mutation(
    doc: &mut Document,
    args: &HashlineArgs,
    content: Option<&str>,
) -> Result<(usize, usize, String), FunctionCallError> {
    match args.action {
        HashlineAction::Edit => {
            let anchor = require_anchor(args)?;
            let content = require_content(args)?;
            let replacement = split_content(content);
            let (start, end) = resolve_anchor_or_range(doc, anchor)?;
            if start == end && replacement.len() > 1 {
                return Err(FunctionCallError::RespondToModel(format!(
                    "fuzz_view_edit action `edit` with multi-line `content` requires an anchor range such as `12:abcd..15:ef01`; a single-line anchor would only replace line {} and can leave stale trailing lines. Use a range to replace an existing block, or delete the old range before inserting new content.",
                    start + 1
                )));
            }
            if replacement.is_empty() {
                doc.lines.drain(start..=end);
            } else {
                doc.lines.splice(start..=end, replacement.iter().cloned());
            }
            let changed_end = if replacement.is_empty() {
                start.min(doc.lines.len().saturating_sub(1))
            } else {
                start + replacement.len().saturating_sub(1)
            };
            Ok((start, changed_end, "Edited".to_string()))
        }
        HashlineAction::Insert => {
            let anchor = require_anchor(args)?;
            let content = content.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "fuzz_view_edit action `insert` requires `content`".to_string(),
                )
            })?;
            let resolved = resolve_anchor(
                doc,
                parse_anchor(anchor, /*allow_bare_line*/ false)?,
                ResolveMode::Strict,
            )?;
            let inserted = split_content(content);
            let insert_at = if args.before {
                resolved.index
            } else {
                resolved.index + 1
            };
            doc.lines
                .splice(insert_at..insert_at, inserted.iter().cloned());
            let changed_end = insert_at + inserted.len().saturating_sub(1);
            Ok((insert_at, changed_end, "Inserted".to_string()))
        }
        HashlineAction::Delete => {
            let anchor = require_anchor(args)?;
            let (start, end) = resolve_anchor_or_range(doc, anchor)?;
            doc.lines.drain(start..=end);
            let snippet_index = start.min(doc.lines.len().saturating_sub(1));
            Ok((snippet_index, snippet_index, "Deleted".to_string()))
        }
        HashlineAction::Read => Err(FunctionCallError::RespondToModel(
            "internal fuzz_view_edit mutation dispatch error".to_string(),
        )),
    }
}

fn hashline_unified_diff(before: &str, after: &str) -> String {
    TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(1)
        .to_string()
}

fn mutation_output(
    changed: (usize, usize, String),
    doc: &Document,
    context: usize,
    path: Option<&PathUri>,
) -> String {
    let (start, end, verb) = changed;
    let (snippet_start, snippet_end) = context_window(doc.lines.len(), start, end, context);
    let path_suffix = path
        .map(|path| format!(" in `{}`", path_display(path)))
        .unwrap_or_default();
    format!(
        "{verb}{path_suffix}.\n{}",
        format_lines(doc, snippet_start, snippet_end, None)
    )
}

fn split_content(content: &str) -> Vec<String> {
    let normalized = content.strip_suffix('\n').unwrap_or(content);
    if normalized.is_empty() {
        Vec::new()
    } else {
        normalized
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
            .collect()
    }
}

fn resolve_anchor_or_range(
    doc: &Document,
    anchor_or_range: &str,
) -> Result<(usize, usize), FunctionCallError> {
    if let Some((left, right)) = anchor_or_range.split_once("..") {
        if right.contains("..") {
            return Err(invalid_anchor(anchor_or_range));
        }
        reject_bare_line_write_anchor(left, anchor_or_range)?;
        reject_bare_line_write_anchor(right, anchor_or_range)?;
        let start = resolve_anchor(
            doc,
            parse_anchor(left, /*allow_bare_line*/ false)?,
            ResolveMode::Strict,
        )?;
        let end = resolve_anchor(
            doc,
            parse_anchor(right, /*allow_bare_line*/ false)?,
            ResolveMode::Strict,
        )?;
        if start.index > end.index {
            return Err(FunctionCallError::RespondToModel(format!(
                "fuzz_view_edit range `{anchor_or_range}` resolves backwards"
            )));
        }
        return Ok((start.index, end.index));
    }
    reject_bare_line_write_anchor(anchor_or_range, anchor_or_range)?;
    let resolved = resolve_anchor(
        doc,
        parse_anchor(anchor_or_range, /*allow_bare_line*/ false)?,
        ResolveMode::Strict,
    )?;
    Ok((resolved.index, resolved.index))
}

fn reject_bare_line_write_anchor(
    anchor: &str,
    anchor_or_range: &str,
) -> Result<(), FunctionCallError> {
    if anchor
        .trim()
        .parse::<usize>()
        .ok()
        .is_some_and(|line| line > 0)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "invalid fuzz_view_edit write anchor `{anchor_or_range}`; edit/insert/delete require `LINE:HASH` anchors or unique bare hashes, not bare line numbers"
        )));
    }
    Ok(())
}

fn parse_anchor(anchor: &str, allow_bare_line: bool) -> Result<Anchor, FunctionCallError> {
    let anchor = anchor.trim();
    if anchor.is_empty() || anchor.contains("..") {
        return Err(invalid_anchor(anchor));
    }
    if let Some((line, hash)) = anchor.split_once(':') {
        let line = line
            .parse::<usize>()
            .ok()
            .filter(|line| *line > 0)
            .ok_or_else(|| invalid_anchor(anchor))?;
        return Ok(Anchor::LineHash {
            line,
            hash: parse_hash(hash, anchor)?,
        });
    }
    if allow_bare_line && let Some(line) = anchor.parse::<usize>().ok().filter(|line| *line > 0) {
        return Ok(Anchor::Line(line));
    }
    Ok(Anchor::Hash(parse_hash(anchor, anchor)?))
}

fn parse_hash(hash: &str, anchor: &str) -> Result<LineHash, FunctionCallError> {
    if !matches!(hash.len(), 2 | 4) || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_anchor(anchor));
    }
    if hash.len() == 2 {
        return u8::from_str_radix(hash, 16)
            .map(LineHash::Short)
            .map_err(|_| invalid_anchor(anchor));
    }
    u16::from_str_radix(hash, 16)
        .map(LineHash::Full)
        .map_err(|_| invalid_anchor(anchor))
}

fn invalid_anchor(anchor: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "invalid fuzz_view_edit anchor `{anchor}`; expected `12:abcd`, `abcd`, `12` for read only, or `12:abcd..15:ef01`"
    ))
}

fn resolve_anchor(
    doc: &Document,
    anchor: Anchor,
    mode: ResolveMode,
) -> Result<ResolvedAnchor, FunctionCallError> {
    let index = doc.hash_index();
    match anchor {
        Anchor::Hash(hash) => match index.get(&hash).map(Vec::as_slice).unwrap_or(&[]) {
            [] => Err(FunctionCallError::RespondToModel(format!(
                "fuzz_view_edit hash `{}` was not found",
                format_short_hash(hash)
            ))),
            [line] => Ok(ResolvedAnchor { index: *line, hash }),
            matches => Err(FunctionCallError::RespondToModel(format!(
                "fuzz_view_edit hash `{}` is ambiguous; it appears on lines {}",
                format_short_hash(hash),
                join_line_numbers(matches)
            ))),
        },
        Anchor::LineHash { line, hash } => {
            let requested_index = line - 1;
            if doc
                .lines
                .get(requested_index)
                .is_some_and(|_| hash.matches(doc.hash_at(requested_index)))
            {
                return Ok(ResolvedAnchor {
                    index: requested_index,
                    hash,
                });
            }
            if mode == ResolveMode::Strict {
                return Err(stale_anchor(doc, line, hash));
            }

            match relocate_anchor(
                requested_index,
                index.get(&hash).map(Vec::as_slice).unwrap_or(&[]),
            ) {
                Some(index) => Ok(ResolvedAnchor { index, hash }),
                None => Err(stale_anchor(doc, line, hash)),
            }
        }
        Anchor::Line(line) => {
            let index = line - 1;
            if doc.lines.get(index).is_none() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "fuzz_view_edit line {line} is out of range (file has {} lines)",
                    doc.lines.len()
                )));
            }
            Ok(ResolvedAnchor {
                index,
                hash: doc.hash_at(index),
            })
        }
    }
}

fn relocate_anchor(requested_index: usize, candidates: &[usize]) -> Option<usize> {
    match candidates {
        [] => None,
        [single] => (single.abs_diff(requested_index) <= FUZZY_RELOCATE_RADIUS).then_some(*single),
        many => {
            let closest = many
                .iter()
                .min_by_key(|candidate| candidate.abs_diff(requested_index))
                .copied()?;
            (closest.abs_diff(requested_index) <= FUZZY_RELOCATE_RADIUS).then_some(closest)
        }
    }
}

fn stale_anchor(doc: &Document, line: usize, expected_hash: LineHash) -> FunctionCallError {
    let requested_index = line.saturating_sub(1);
    let actual = doc
        .lines
        .get(requested_index)
        .map(|_| format_short_hash(doc.hash_at(requested_index)))
        .unwrap_or_else(|| "missing".to_string());
    let (start, end) = context_window(doc.lines.len(), requested_index, requested_index, 2);
    FunctionCallError::RespondToModel(format!(
        "line {line} content changed since last read (expected hash {}, got {actual})\n{}",
        format_short_hash(expected_hash),
        format_lines(doc, start, end, Some(requested_index))
    ))
}

fn context_window(
    line_count: usize,
    start_index: usize,
    end_index: usize,
    context: usize,
) -> (usize, usize) {
    if line_count == 0 {
        return (0, 0);
    }
    let start = start_index.saturating_sub(context);
    let end = (end_index + context + 1).min(line_count);
    (start, end)
}

fn format_lines(doc: &Document, start: usize, end: usize, marker_index: Option<usize>) -> String {
    if doc.lines.is_empty() {
        return "<empty file>".to_string();
    }
    let mut output = String::new();
    for index in start..end {
        if marker_index == Some(index) {
            output.push_str(">>> ");
        }
        output.push_str(&format!(
            "{}:{}|{}\n",
            index + 1,
            format_short_hash(doc.hash_at(index)),
            format_line_content(&doc.lines[index])
        ));
    }
    output
}

fn format_line_content(line: &str) -> String {
    let mut chars = line.chars();
    let truncated: String = chars.by_ref().take(MAX_FORMATTED_LINE_CHARS).collect();
    if chars.next().is_some() {
        let total_chars = MAX_FORMATTED_LINE_CHARS + 1 + chars.count();
        format!(
            "{truncated}... [truncated to first {MAX_FORMATTED_LINE_CHARS} of {total_chars} chars]"
        )
    } else {
        truncated
    }
}

fn join_line_numbers(indices: &[usize]) -> String {
    indices
        .iter()
        .map(|index| (index + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn full_hash(line: &str) -> LineHash {
    LineHash::Full((xxh32(line.trim_end().as_bytes(), 0) & 0xffff) as u16)
}

#[cfg(test)]
fn short_hash(line: &str) -> LineHash {
    full_hash(line).short()
}

fn format_short_hash(hash: LineHash) -> String {
    match hash {
        LineHash::Short(hash) => format!("{hash:02x}"),
        LineHash::Full(hash) => format!("{hash:04x}"),
    }
}

impl LineHash {
    fn short(self) -> Self {
        match self {
            LineHash::Short(hash) => LineHash::Short(hash),
            LineHash::Full(hash) => LineHash::Short((hash & 0xff) as u8),
        }
    }

    fn matches(self, actual: Self) -> bool {
        match self {
            LineHash::Short(_) => self == actual.short(),
            LineHash::Full(_) => self == actual,
        }
    }
}

fn xxh32(input: &[u8], seed: u32) -> u32 {
    const PRIME32_1: u32 = 0x9e37_79b1;
    const PRIME32_2: u32 = 0x85eb_ca77;
    const PRIME32_3: u32 = 0xc2b2_ae3d;
    const PRIME32_4: u32 = 0x27d4_eb2f;
    const PRIME32_5: u32 = 0x1656_67b1;

    let mut offset = 0;
    let len = input.len() as u32;
    let mut hash;
    if input.len() >= 16 {
        let mut v1 = seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
        let mut v2 = seed.wrapping_add(PRIME32_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME32_1);
        while offset <= input.len() - 16 {
            v1 = round(v1, read_u32(input, offset));
            v2 = round(v2, read_u32(input, offset + 4));
            v3 = round(v3, read_u32(input, offset + 8));
            v4 = round(v4, read_u32(input, offset + 12));
            offset += 16;
        }
        hash = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
    } else {
        hash = seed.wrapping_add(PRIME32_5);
    }

    hash = hash.wrapping_add(len);
    while offset + 4 <= input.len() {
        hash = hash
            .wrapping_add(read_u32(input, offset).wrapping_mul(PRIME32_3))
            .rotate_left(17)
            .wrapping_mul(PRIME32_4);
        offset += 4;
    }
    while offset < input.len() {
        hash = hash
            .wrapping_add(u32::from(input[offset]).wrapping_mul(PRIME32_5))
            .rotate_left(11)
            .wrapping_mul(PRIME32_1);
        offset += 1;
    }
    avalanche(hash)
}

fn round(acc: u32, input: u32) -> u32 {
    const PRIME32_1: u32 = 0x9e37_79b1;
    const PRIME32_2: u32 = 0x85eb_ca77;

    acc.wrapping_add(input.wrapping_mul(PRIME32_2))
        .rotate_left(13)
        .wrapping_mul(PRIME32_1)
}

fn avalanche(mut hash: u32) -> u32 {
    const PRIME32_2: u32 = 0x85eb_ca77;
    const PRIME32_3: u32 = 0xc2b2_ae3d;

    hash ^= hash >> 15;
    hash = hash.wrapping_mul(PRIME32_2);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(PRIME32_3);
    hash ^= hash >> 16;
    hash
}

fn read_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ])
}

#[cfg(test)]
#[path = "hashline_tests.rs"]
mod tests;
