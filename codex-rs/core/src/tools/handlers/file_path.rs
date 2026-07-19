use crate::function_tool::FunctionCallError;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use codex_utils_string::truncate_middle_chars;

const MAX_ARGUMENT_DISPLAY_BYTES: usize = 1024;
const MAX_MODEL_ERROR_BYTES: usize = 2048;

pub(super) fn summarize_tool_argument(value: &str) -> String {
    truncate_middle_chars(value, MAX_ARGUMENT_DISPLAY_BYTES)
}

pub(super) fn bounded_model_error(message: impl AsRef<str>) -> FunctionCallError {
    FunctionCallError::RespondToModel(truncate_middle_chars(
        message.as_ref(),
        MAX_MODEL_ERROR_BYTES,
    ))
}

pub(super) fn bound_function_call_error(error: FunctionCallError) -> FunctionCallError {
    match error {
        FunctionCallError::RespondToModel(message) => bounded_model_error(message),
        FunctionCallError::Fatal(message) => FunctionCallError::Fatal(message),
    }
}

pub(super) fn resolve_tool_path(
    tool_name: &str,
    argument_name: &str,
    cwd: &PathUri,
    path: &str,
) -> Result<PathUri, FunctionCallError> {
    if path.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} requires a non-empty {argument_name}"
        )));
    }
    if path.contains(['\n', '\r']) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} {argument_name} cannot contain a newline"
        )));
    }
    if let Ok(uri) = PathUri::parse(path) {
        return Ok(uri);
    }

    let legacy_path: LegacyAppPathString =
        serde_json::from_value(serde_json::Value::String(path.to_string())).map_err(|error| {
            let path = summarize_tool_argument(path);
            bounded_model_error(format!(
                "{tool_name} {argument_name} `{path}` is not valid path text: {error}"
            ))
        })?;
    if let Some(convention) = legacy_path.infer_absolute_path_convention() {
        return legacy_path.to_path_uri(convention).map_err(|error| {
            let path = summarize_tool_argument(path);
            bounded_model_error(format!(
                "{tool_name} {argument_name} `{path}` is not a valid absolute {convention} path: {error}"
            ))
        });
    }
    cwd.join(&path.replace('\\', "/")).map_err(|error| {
        let path = summarize_tool_argument(path);
        bounded_model_error(format!(
            "{tool_name} failed to resolve `{path}` against `{cwd}`: {error}"
        ))
    })
}

pub(super) fn is_absolute_file_path(path: &str) -> bool {
    if PathUri::parse(path).is_ok() {
        return true;
    }
    serde_json::from_value::<LegacyAppPathString>(serde_json::Value::String(path.to_string()))
        .ok()
        .and_then(|path| path.infer_absolute_path_convention())
        .is_some()
}

#[cfg(test)]
#[path = "file_path_tests.rs"]
mod tests;

pub(super) fn is_blocked_device_path(path: &str) -> bool {
    matches!(
        path,
        "/dev/zero"
            | "/dev/random"
            | "/dev/urandom"
            | "/dev/full"
            | "/dev/stdin"
            | "/dev/tty"
            | "/dev/console"
            | "/dev/stdout"
            | "/dev/stderr"
            | "/dev/fd/0"
            | "/dev/fd/1"
            | "/dev/fd/2"
    ) || (path.starts_with("/proc/")
        && ["/fd/0", "/fd/1", "/fd/2"]
            .iter()
            .any(|suffix| path.ends_with(suffix)))
}

pub(super) fn blocked_binary_extension(path: &str) -> Option<&'static str> {
    const BLOCKED_EXTENSIONS: &[&str] = &[
        ".bmp", ".ico", ".tiff", ".tif", ".mp4", ".mov", ".avi", ".mkv", ".webm", ".wmv", ".flv",
        ".m4v", ".mpeg", ".mpg", ".mp3", ".wav", ".ogg", ".flac", ".aac", ".m4a", ".wma", ".aiff",
        ".opus", ".zip", ".tar", ".gz", ".bz2", ".7z", ".rar", ".xz", ".z", ".tgz", ".iso", ".exe",
        ".dll", ".so", ".dylib", ".bin", ".o", ".a", ".obj", ".lib", ".app", ".msi", ".deb",
        ".rpm", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".odt", ".ods", ".odp", ".ttf",
        ".otf", ".woff", ".woff2", ".eot", ".pyc", ".pyo", ".class", ".jar", ".war", ".ear",
        ".node", ".wasm", ".rlib", ".sqlite", ".sqlite3", ".db", ".mdb", ".idx", ".psd", ".ai",
        ".eps", ".sketch", ".fig", ".xd", ".blend", ".3ds", ".max", ".swf", ".fla", ".lockb",
        ".dat", ".data",
    ];

    let lower = path.to_ascii_lowercase();
    BLOCKED_EXTENSIONS
        .iter()
        .copied()
        .find(|extension| lower.ends_with(extension))
}
