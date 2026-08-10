use codex_file_system::GetMetadataOptions;
use codex_file_system::WalkEntryKind;
use codex_file_system::WalkOptions;
use codex_tools::ToolExecutionEnvironment;
use codex_utils_path_uri::PathConvention;
use codex_workflow::WorkflowDeclaredInputFile;
use codex_workflow::WorkflowDeclaredInputs;
use futures::StreamExt;
use globset::GlobBuilder;
use globset::GlobMatcher;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::io;

const MAX_INPUT_PATTERNS: usize = 64;
const MAX_INPUT_PATTERN_BYTES: usize = 512;
const MAX_INPUT_FILES: usize = 256;
const MAX_INPUT_FILE_BYTES: usize = 256 * 1024;
const MAX_INPUT_TOTAL_BYTES: usize = 2 * 1024 * 1024;
const MAX_INPUT_WALK_DEPTH: usize = 64;
const MAX_INPUT_WALK_DIRECTORIES: usize = 1_024;
const MAX_INPUT_WALK_ENTRIES: usize = 4_096;

struct DeclaredPattern {
    source: String,
    matcher: GlobMatcher,
    scan_root: String,
    max_depth: usize,
    exact: bool,
}

pub(crate) async fn freeze_declared_inputs(
    patterns: &[String],
    environments: &[ToolExecutionEnvironment],
) -> Result<WorkflowDeclaredInputs, String> {
    if patterns.is_empty() {
        return Ok(WorkflowDeclaredInputs::default());
    }
    if patterns.len() > MAX_INPUT_PATTERNS {
        return Err(format!(
            "meta.inputs supports at most {MAX_INPUT_PATTERNS} patterns"
        ));
    }
    let [environment] = environments else {
        return Err(
            "meta.inputs requires exactly one selected execution environment; use agent tools for multi-environment file access"
                .to_string(),
        );
    };
    let declared = patterns
        .iter()
        .map(|pattern| {
            declared_pattern(
                pattern,
                environment.cwd.infer_path_convention() == Some(PathConvention::Windows),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut candidates = BTreeMap::new();
    let mut examined_entries = 0usize;

    for pattern in &declared {
        let mut matched = false;
        if pattern.exact {
            let path = environment.cwd.join(&pattern.source).map_err(|error| {
                format!("invalid meta.inputs path `{}`: {error}", pattern.source)
            })?;
            match environment
                .file_system
                .get_metadata(
                    &path,
                    GetMetadataOptions {
                        follow_symlinks: true,
                    },
                    Some(&environment.file_system_sandbox_context),
                )
                .await
            {
                Ok(metadata) if metadata.is_file && !metadata.is_symlink => {
                    candidates.insert(pattern.source.clone(), path);
                    matched = true;
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "failed to inspect meta.inputs path `{}`: {error}",
                        pattern.source
                    ));
                }
            }
        } else {
            let scan_root = environment.cwd.join(&pattern.scan_root).map_err(|error| {
                format!("invalid meta.inputs path `{}`: {error}", pattern.source)
            })?;
            let remaining_entries = MAX_INPUT_WALK_ENTRIES.saturating_sub(examined_entries);
            if remaining_entries == 0 {
                return Err(format!(
                    "meta.inputs traversal exceeds {MAX_INPUT_WALK_ENTRIES} entries"
                ));
            }
            let outcome = match environment
                .file_system
                .walk(
                    &scan_root,
                    WalkOptions {
                        max_depth: pattern.max_depth,
                        max_directories: MAX_INPUT_WALK_DIRECTORIES,
                        max_entries: remaining_entries,
                        follow_directory_symlinks: false,
                        prune_hidden_directories: false,
                    },
                    Some(&environment.file_system_sandbox_context),
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(format!(
                        "meta.inputs pattern `{}` matched no files",
                        pattern.source
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "failed to expand meta.inputs pattern `{}`: {error}",
                        pattern.source
                    ));
                }
            };
            examined_entries = examined_entries.saturating_add(outcome.entries.len());
            if outcome.truncated || examined_entries > MAX_INPUT_WALK_ENTRIES {
                return Err(format!(
                    "meta.inputs traversal exceeds {MAX_INPUT_WALK_ENTRIES} entries"
                ));
            }
            if let Some(error) = outcome.errors.first() {
                return Err(format!(
                    "failed to inspect declared input `{}`: {}",
                    error.path, error.message
                ));
            }
            for entry in outcome.entries {
                if entry.kind != WalkEntryKind::File {
                    continue;
                }
                let Some(relative) = entry.path.relative_path_from(&environment.cwd) else {
                    return Err(format!(
                        "declared input `{}` is outside the selected workspace",
                        entry.path
                    ));
                };
                let relative = relative.replace('\\', "/");
                if pattern.matcher.is_match(&relative) {
                    candidates.insert(relative, entry.path);
                    matched = true;
                }
            }
        }
        if !matched {
            return Err(format!(
                "meta.inputs pattern `{}` matched no files",
                pattern.source
            ));
        }
        if candidates.len() > MAX_INPUT_FILES {
            return Err(format!(
                "meta.inputs matches more than {MAX_INPUT_FILES} files"
            ));
        }
    }

    let mut files = BTreeMap::new();
    let mut total_bytes = 0usize;
    for (relative, path) in candidates {
        let before = environment
            .file_system
            .get_metadata(
                &path,
                GetMetadataOptions {
                    follow_symlinks: true,
                },
                Some(&environment.file_system_sandbox_context),
            )
            .await
            .map_err(|error| {
                format!("failed to inspect declared input `{relative}` at `{path}`: {error}")
            })?;
        if !before.is_file || before.is_symlink {
            return Err(format!(
                "declared input `{relative}` must be a regular file"
            ));
        }
        let file_bytes = usize::try_from(before.size).unwrap_or(usize::MAX);
        if file_bytes > MAX_INPUT_FILE_BYTES {
            return Err(format!(
                "declared input `{relative}` exceeds {MAX_INPUT_FILE_BYTES} bytes"
            ));
        }
        if file_bytes > MAX_INPUT_TOTAL_BYTES.saturating_sub(total_bytes) {
            return Err(format!(
                "meta.inputs exceeds the {MAX_INPUT_TOTAL_BYTES} byte total limit"
            ));
        }
        let mut stream = environment
            .file_system
            .read_file_stream(&path, Some(&environment.file_system_sandbox_context))
            .await
            .map_err(|error| format!("failed to read declared input `{relative}`: {error}"))?;
        let mut bytes = Vec::with_capacity(file_bytes);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|error| format!("failed to read declared input `{relative}`: {error}"))?;
            if chunk.len() > MAX_INPUT_FILE_BYTES.saturating_sub(bytes.len()) {
                return Err(format!(
                    "declared input `{relative}` exceeds {MAX_INPUT_FILE_BYTES} bytes"
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let after = environment
            .file_system
            .get_metadata(
                &path,
                GetMetadataOptions {
                    follow_symlinks: true,
                },
                Some(&environment.file_system_sandbox_context),
            )
            .await
            .map_err(|error| format!("failed to recheck declared input `{relative}`: {error}"))?;
        if before != after || bytes.len() != file_bytes {
            return Err(format!(
                "declared input `{relative}` changed while it was being frozen"
            ));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| format!("declared input `{relative}` must contain valid UTF-8 text"))?;
        total_bytes = total_bytes.saturating_add(content.len());
        files.insert(
            relative,
            WorkflowDeclaredInputFile {
                sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
                bytes: content.len(),
                content,
            },
        );
    }
    Ok(WorkflowDeclaredInputs {
        patterns: patterns.to_vec(),
        files,
    })
}

fn declared_pattern(pattern: &str, case_insensitive: bool) -> Result<DeclaredPattern, String> {
    if pattern.is_empty() || pattern.len() > MAX_INPUT_PATTERN_BYTES {
        return Err(format!(
            "meta.inputs patterns must contain 1 to {MAX_INPUT_PATTERN_BYTES} bytes"
        ));
    }
    if pattern.starts_with(['/', '\\', '~'])
        || pattern.as_bytes().get(1) == Some(&b':')
        || pattern.contains('\\')
        || pattern
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
    {
        return Err(format!(
            "meta.inputs pattern `{pattern}` must be workspace-relative and use `/` separators"
        ));
    }
    let exact = !pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'{' | b'!'));
    let mut builder = GlobBuilder::new(pattern);
    builder
        .literal_separator(true)
        .backslash_escape(false)
        .case_insensitive(case_insensitive);
    let matcher = builder
        .build()
        .map_err(|error| format!("invalid meta.inputs pattern `{pattern}`: {error}"))?
        .compile_matcher();
    let components = pattern.split('/').collect::<Vec<_>>();
    let literal_components = components
        .iter()
        .take_while(|component| {
            !component
                .bytes()
                .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'{' | b'!'))
        })
        .copied()
        .collect::<Vec<_>>();
    let scan_root = literal_components.join("/");
    let remaining = components.len().saturating_sub(literal_components.len());
    let max_depth = if components.contains(&"**") {
        MAX_INPUT_WALK_DEPTH
    } else {
        remaining.max(1)
    };
    Ok(DeclaredPattern {
        source: pattern.to_string(),
        matcher,
        scan_root,
        max_depth,
        exact,
    })
}

#[cfg(test)]
#[path = "declared_inputs_tests.rs"]
mod tests;
