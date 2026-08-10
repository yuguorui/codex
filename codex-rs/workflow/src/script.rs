use crate::MAX_WORKFLOW_PROGRESS_TEXT_BYTES;
use crate::WorkflowChildReference;
use crate::WorkflowMeta;
use crate::scope_analysis::UNAVAILABLE_GLOBAL_NAMES;
use crate::scope_analysis::WorkflowBodyAnalysisError;
use crate::scope_analysis::analyze_workflow_body;

pub const MAX_WORKFLOW_SCRIPT_BYTES: usize = 512 * 1024;
const MAX_PHASES: usize = 4096;
const MAX_WORKFLOW_NAME_BYTES: usize = 128;
const MAX_WORKFLOW_TITLE_BYTES: usize = 256;
const META_PREFIX: &str = "export const meta";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedWorkflowScript {
    pub source: String,
    pub body: String,
    pub meta: WorkflowMeta,
    pub child_references: Vec<WorkflowChildReference>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkflowScriptError {
    #[error("provide a focused workflow script and move reusable stages into child workflows")]
    TooLarge,
    #[error("workflow script text must use supported source characters; check byte {0}")]
    ControlCharacter(usize),
    #[error("the first statement must be `export const meta = {{...}}`")]
    MissingMeta,
    #[error("workflow metadata must be a pure object literal: {0}")]
    InvalidMeta(String),
    #[error("workflow metadata field `{0}` must be non-empty")]
    EmptyMetaField(&'static str),
    #[error("use a concise workflow metadata field `{field}")]
    MetaFieldTooLarge {
        field: &'static str,
        max_bytes: usize,
    },
    #[error("use a focused set of workflow phases")]
    TooManyPhases,
    #[error("provide `{0}` values through workflow args for deterministic execution")]
    Nondeterministic(&'static str),
    #[error("workflow script uses reserved internal identifier `{0}`")]
    ReservedIdentifier(String),
    #[error("workflow script uses an escaped identifier at byte {0}")]
    EscapedIdentifier(usize),
    #[error(
        "workflow script has an invalid `agent()` prompt at line {line}, column {column}: {reason}"
    )]
    InvalidAgentPrompt {
        line: usize,
        column: usize,
        reason: &'static str,
    },
    #[error(
        "workflow script has an invalid child workflow reference at line {line}, column {column}: {reason}"
    )]
    InvalidWorkflowReference {
        line: usize,
        column: usize,
        reason: &'static str,
    },
    #[error(
        "workflow script should use its runtime API directly; `{name}` at line {line}, column {column} is an outer API. {guidance}"
    )]
    UnavailableGlobal {
        name: String,
        line: usize,
        column: usize,
        guidance: &'static str,
    },
}

pub fn validate_workflow_script(
    source: impl Into<String>,
) -> Result<ValidatedWorkflowScript, WorkflowScriptError> {
    let source = source.into();
    validate_size_and_characters(&source)?;
    let (meta_literal, body) = split_meta_statement(&source)?;
    let json5_meta = normalize_template_strings(meta_literal)?;
    let meta: WorkflowMeta = json5::from_str(&json5_meta)
        .map_err(|error| WorkflowScriptError::InvalidMeta(error.to_string()))?;
    validate_meta(&meta)?;
    validate_determinism(body)?;
    validate_reserved_identifiers(body)?;
    let body_start = source.len() - body.len();
    let analysis = analyze_workflow_body(body).map_err(|error| match error {
        WorkflowBodyAnalysisError::InvalidAgentPrompt(invalid) => {
            let (line, column) = line_and_column(&source, body_start + invalid.byte_offset);
            WorkflowScriptError::InvalidAgentPrompt {
                line,
                column,
                reason: invalid.reason,
            }
        }
        WorkflowBodyAnalysisError::InvalidWorkflowReference(invalid) => {
            let (line, column) = line_and_column(&source, body_start + invalid.byte_offset);
            WorkflowScriptError::InvalidWorkflowReference {
                line,
                column,
                reason: invalid.reason,
            }
        }
    })?;
    if let Some(unavailable) = analysis.unavailable_global {
        let (line, column) = line_and_column(&source, body_start + unavailable.byte_offset);
        return Err(WorkflowScriptError::UnavailableGlobal {
            name: unavailable.name,
            line,
            column,
            guidance: unavailable.guidance,
        });
    }
    let body = body.to_string();
    Ok(ValidatedWorkflowScript {
        source,
        body,
        meta,
        child_references: analysis.child_references,
    })
}

fn line_and_column(source: &str, byte_offset: usize) -> (usize, usize) {
    let before = &source[..byte_offset];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = before
        .rsplit_once('\n')
        .map_or(before, |(_, tail)| tail)
        .chars()
        .count()
        + 1;
    (line, column)
}

pub fn compile_workflow_source(
    script: &ValidatedWorkflowScript,
    args: &serde_json::Value,
) -> Result<String, WorkflowScriptError> {
    compile_workflow_source_with_context(script, args, WorkflowScriptContext::default())
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkflowScriptContext {
    pub(crate) child_mode: bool,
    pub(crate) phase_index: Option<usize>,
    pub(crate) phase_title: Option<String>,
    pub(crate) result_tool_name: Option<String>,
}

pub(crate) fn compile_workflow_source_with_context(
    script: &ValidatedWorkflowScript,
    args: &serde_json::Value,
    context: WorkflowScriptContext,
) -> Result<String, WorkflowScriptError> {
    let args = serde_json::to_string(args)
        .map_err(|error| WorkflowScriptError::InvalidMeta(error.to_string()))?;
    let phase_titles = script
        .meta
        .phases
        .iter()
        .map(|phase| phase.title.as_str())
        .collect::<Vec<_>>();
    let phase_titles = serde_json::to_string(&phase_titles)
        .map_err(|error| WorkflowScriptError::InvalidMeta(error.to_string()))?;
    let phase_index = context
        .phase_index
        .map_or_else(|| "null".to_string(), |index| index.to_string());
    let phase_title = serde_json::to_string(&context.phase_title)
        .map_err(|error| WorkflowScriptError::InvalidMeta(error.to_string()))?;
    let result_tool_name = serde_json::to_string(
        context
            .result_tool_name
            .as_deref()
            .unwrap_or("workflow_result"),
    )
    .map_err(|error| WorkflowScriptError::InvalidMeta(error.to_string()))?;
    let unavailable_global_names = serde_json::to_string(UNAVAILABLE_GLOBAL_NAMES)
        .map_err(|error| WorkflowScriptError::InvalidMeta(error.to_string()))?;
    Ok(format!(
        "const __wfArgsJson = {};\nconst __wfPhaseTitlesJson = {};\nconst __wfChildMode = {};\nconst __wfInitialPhaseIndex = {};\nconst __wfInitialPhaseTitle = {};\nconst __wfResultToolName = {};\nconst __wfUnavailableGlobalNames = {unavailable_global_names};\n{}\n{}\n",
        serde_json::to_string(&args)
            .map_err(|error| WorkflowScriptError::InvalidMeta(error.to_string()))?,
        serde_json::to_string(&phase_titles)
            .map_err(|error| WorkflowScriptError::InvalidMeta(error.to_string()))?,
        context.child_mode,
        phase_index,
        phase_title,
        result_tool_name,
        include_str!("prelude.js"),
        workflow_body(&script.body),
    ))
}

fn workflow_body(body: &str) -> String {
    format!(
        r#"
const args = __wfDeepFreeze(JSON.parse(__wfArgsJson));
const {{ agent, agentSettled, parallel, pipeline, phase, log, workflow, listInputs, readInput }} = __wfBuildApi();
const console = Object.freeze({{ log, info: log, warn: log, error: log }});
const __wfMain = async () => {{
{body}
}};
const __wfResult = await __wfMain();
await __wfHostResult({{ result: __wfSanitize(__wfResult), tokens: __wfRunTokens }});
"#
    )
}

fn validate_size_and_characters(source: &str) -> Result<(), WorkflowScriptError> {
    if source.len() > MAX_WORKFLOW_SCRIPT_BYTES {
        return Err(WorkflowScriptError::TooLarge);
    }
    if let Some((index, _)) = source
        .char_indices()
        .find(|(_, character)| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(WorkflowScriptError::ControlCharacter(index));
    }
    Ok(())
}

fn split_meta_statement(source: &str) -> Result<(&str, &str), WorkflowScriptError> {
    let trimmed = source.trim_start_matches(char::is_whitespace);
    let rest = trimmed
        .strip_prefix(META_PREFIX)
        .ok_or(WorkflowScriptError::MissingMeta)?;
    let rest = rest.trim_start_matches(char::is_whitespace);
    let rest = rest
        .strip_prefix('=')
        .ok_or(WorkflowScriptError::MissingMeta)?
        .trim_start_matches(char::is_whitespace);
    if !rest.starts_with('{') {
        return Err(WorkflowScriptError::MissingMeta);
    }
    let object_end = find_object_end(rest)?;
    let meta_literal = &rest[..=object_end];
    let mut body = &rest[object_end + 1..];
    body = body.trim_start_matches(char::is_whitespace);
    if let Some(after_semicolon) = body.strip_prefix(';') {
        body = after_semicolon;
    }
    Ok((meta_literal, body))
}

fn find_object_end(input: &str) -> Result<usize, WorkflowScriptError> {
    let bytes = input.as_bytes();
    let mut index = 0;
    let mut depth = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => index = skip_quoted(bytes, index, bytes[index])?,
            b'`' => index = skip_template(bytes, index)?,
            b'/' if bytes.get(index + 1) == Some(&b'/') => index = skip_line_comment(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index)?;
            }
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1).ok_or_else(invalid_meta_literal)?;
                if depth == 0 {
                    return Ok(index);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    Err(invalid_meta_literal())
}

fn skip_quoted(bytes: &[u8], mut index: usize, quote: u8) -> Result<usize, WorkflowScriptError> {
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            value if value == quote => return Ok(index + 1),
            b'\n' | b'\r' => return Err(invalid_meta_literal()),
            _ => index += 1,
        }
    }
    Err(invalid_meta_literal())
}

fn skip_template(bytes: &[u8], mut index: usize) -> Result<usize, WorkflowScriptError> {
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'`' => return Ok(index + 1),
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                return Err(WorkflowScriptError::InvalidMeta(
                    "template interpolation is not allowed".to_string(),
                ));
            }
            _ => index += 1,
        }
    }
    Err(invalid_meta_literal())
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    index += 2;
    while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
        index += 1;
    }
    index
}

fn skip_block_comment(bytes: &[u8], mut index: usize) -> Result<usize, WorkflowScriptError> {
    index += 2;
    while index + 1 < bytes.len() {
        if bytes[index] == b'*' && bytes[index + 1] == b'/' {
            return Ok(index + 2);
        }
        index += 1;
    }
    Err(invalid_meta_literal())
}

fn invalid_meta_literal() -> WorkflowScriptError {
    WorkflowScriptError::InvalidMeta("unterminated object or string".to_string())
}

fn normalize_template_strings(input: &str) -> Result<String, WorkflowScriptError> {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '`' {
            output.push(character);
            continue;
        }
        output.push('"');
        let mut closed = false;
        while let Some(template_character) = chars.next() {
            match template_character {
                '`' => {
                    output.push('"');
                    closed = true;
                    break;
                }
                '$' if chars.peek() == Some(&'{') => {
                    return Err(WorkflowScriptError::InvalidMeta(
                        "template interpolation is not allowed".to_string(),
                    ));
                }
                '\\' => match chars.next() {
                    Some('`') => output.push('`'),
                    Some('\n') => {}
                    Some('\r') => {
                        if chars.peek() == Some(&'\n') {
                            chars.next();
                        }
                    }
                    Some(escaped) => {
                        output.push('\\');
                        output.push(escaped);
                    }
                    None => return Err(invalid_meta_literal()),
                },
                '"' => output.push_str("\\\""),
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                value => output.push(value),
            }
        }
        if !closed {
            return Err(invalid_meta_literal());
        }
    }
    Ok(output)
}

fn validate_meta(meta: &WorkflowMeta) -> Result<(), WorkflowScriptError> {
    require_nonempty("name", &meta.name)?;
    require_bounded("name", &meta.name, MAX_WORKFLOW_NAME_BYTES)?;
    require_nonempty("description", &meta.description)?;
    if let Some(title) = meta.title.as_deref() {
        require_bounded("title", title, MAX_WORKFLOW_TITLE_BYTES)?;
    }
    if meta.phases.len() > MAX_PHASES {
        return Err(WorkflowScriptError::TooManyPhases);
    }
    for phase in &meta.phases {
        require_nonempty("phases[].title", &phase.title)?;
        require_bounded(
            "phases[].title",
            &phase.title,
            MAX_WORKFLOW_PROGRESS_TEXT_BYTES,
        )?;
    }
    Ok(())
}

fn require_bounded(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), WorkflowScriptError> {
    if value.len() > max_bytes {
        Err(WorkflowScriptError::MetaFieldTooLarge { field, max_bytes })
    } else {
        Ok(())
    }
}

fn require_nonempty(field: &'static str, value: &str) -> Result<(), WorkflowScriptError> {
    if value.trim().is_empty() {
        Err(WorkflowScriptError::EmptyMetaField(field))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeterminismToken<'a> {
    Identifier(&'a str),
    Dot,
    OpenParen,
    CloseParen,
    Other,
}

fn validate_determinism(source: &str) -> Result<(), WorkflowScriptError> {
    let tokens = determinism_tokens(source);
    for window in tokens.windows(4) {
        if matches!(
            window,
            [
                DeterminismToken::Identifier("new"),
                DeterminismToken::Identifier("Date"),
                DeterminismToken::OpenParen,
                DeterminismToken::CloseParen,
            ]
        ) {
            return Err(WorkflowScriptError::Nondeterministic("new Date()"));
        }
    }
    for window in tokens.windows(3) {
        match window {
            [
                DeterminismToken::Identifier("Date"),
                DeterminismToken::Dot,
                DeterminismToken::Identifier("now"),
            ] => return Err(WorkflowScriptError::Nondeterministic("Date.now()")),
            [
                DeterminismToken::Identifier("Math"),
                DeterminismToken::Dot,
                DeterminismToken::Identifier("random"),
            ] => return Err(WorkflowScriptError::Nondeterministic("Math.random()")),
            _ => {}
        }
    }
    Ok(())
}

fn validate_reserved_identifiers(source: &str) -> Result<(), WorkflowScriptError> {
    if let Some(identifier) = identifier_tokens_including_templates(source)?
        .into_iter()
        .find(|identifier| identifier.starts_with("__wf"))
    {
        return Err(WorkflowScriptError::ReservedIdentifier(
            identifier.to_string(),
        ));
    }
    Ok(())
}

fn identifier_tokens_including_templates(source: &str) -> Result<Vec<&str>, WorkflowScriptError> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => index = skip_quoted_for_scan(bytes, index, bytes[index]),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(bytes, index);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment_for_scan(bytes, index);
            }
            b'/' if slash_starts_regex(bytes, index) => {
                index = skip_regex_for_scan(bytes, index).unwrap_or(index + 1);
            }
            b'\\' if bytes.get(index + 1) == Some(&b'u') => {
                return Err(WorkflowScriptError::EscapedIdentifier(index));
            }
            value if is_identifier_start(value) => {
                let start = index;
                index += 1;
                while index < bytes.len() && is_identifier_continue(bytes[index]) {
                    index += 1;
                }
                tokens.push(&source[start..index]);
            }
            _ => index += 1,
        }
    }
    Ok(tokens)
}

fn determinism_tokens(source: &str) -> Vec<DeterminismToken<'_>> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => {
                tokens.push(DeterminismToken::Other);
                index = skip_quoted_for_scan(bytes, index, bytes[index]);
            }
            b'`' => {
                tokens.push(DeterminismToken::Other);
                index = skip_template_for_scan(bytes, index);
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => index = skip_line_comment(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment_for_scan(bytes, index);
            }
            b'/' if slash_starts_regex(bytes, index) => {
                tokens.push(DeterminismToken::Other);
                index = skip_regex_for_scan(bytes, index).unwrap_or(index + 1);
            }
            b'.' => {
                tokens.push(DeterminismToken::Dot);
                index += 1;
            }
            b'(' => {
                tokens.push(DeterminismToken::OpenParen);
                index += 1;
            }
            b')' => {
                tokens.push(DeterminismToken::CloseParen);
                index += 1;
            }
            value if is_identifier_start(value) => {
                let start = index;
                index += 1;
                while index < bytes.len() && is_identifier_continue(bytes[index]) {
                    index += 1;
                }
                tokens.push(DeterminismToken::Identifier(&source[start..index]));
            }
            value if value.is_ascii_whitespace() => index += 1,
            _ => {
                tokens.push(DeterminismToken::Other);
                index += 1;
            }
        }
    }
    tokens
}

fn skip_quoted_for_scan(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            value if value == quote => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

fn skip_template_for_scan(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'`' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

fn skip_block_comment_for_scan(bytes: &[u8], mut index: usize) -> usize {
    index += 2;
    while index + 1 < bytes.len() {
        if bytes[index] == b'*' && bytes[index + 1] == b'/' {
            return index + 2;
        }
        index += 1;
    }
    bytes.len()
}

fn slash_starts_regex(bytes: &[u8], slash_index: usize) -> bool {
    let mut end = slash_index;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return true;
    }
    let previous = bytes[end - 1];
    if b"([{=:,;!?&|+-*%^~<>".contains(&previous) {
        return true;
    }
    if !is_identifier_continue(previous) {
        return false;
    }
    let mut start = end - 1;
    while start > 0 && is_identifier_continue(bytes[start - 1]) {
        start -= 1;
    }
    matches!(
        &bytes[start..end],
        b"await"
            | b"case"
            | b"delete"
            | b"do"
            | b"else"
            | b"in"
            | b"instanceof"
            | b"new"
            | b"of"
            | b"return"
            | b"throw"
            | b"typeof"
            | b"void"
            | b"yield"
    )
}

fn skip_regex_for_scan(bytes: &[u8], mut index: usize) -> Option<usize> {
    index += 1;
    let mut in_character_class = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'[' => {
                in_character_class = true;
                index += 1;
            }
            b']' => {
                in_character_class = false;
                index += 1;
            }
            b'/' if !in_character_class => {
                index += 1;
                while index < bytes.len() && is_identifier_continue(bytes[index]) {
                    index += 1;
                }
                return Some(index);
            }
            b'\n' | b'\r' => return None,
            _ => index += 1,
        }
    }
    None
}

fn is_identifier_start(value: u8) -> bool {
    value == b'_' || value == b'$' || value.is_ascii_alphabetic()
}

fn is_identifier_continue(value: u8) -> bool {
    is_identifier_start(value) || value.is_ascii_digit()
}

#[cfg(test)]
#[path = "script_tests.rs"]
mod tests;
