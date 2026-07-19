use codex_apply_patch::AppliedPatchChange;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::AppliedPatchFileChange;
use codex_protocol::protocol::FileChange;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use similar::TextDiff;
use std::collections::HashMap;
use std::path::PathBuf;

pub(super) const MAX_FILE_CHANGE_DIFF_INPUT_BYTES: usize = 2 * 1024 * 1024;

pub(super) struct FileChangeEvent {
    pub changes: HashMap<PathBuf, FileChange>,
    pub delta: Option<AppliedPatchDelta>,
}

pub(super) fn file_change_event(
    path: &PathUri,
    original: Option<&str>,
    updated: &str,
    context_radius: usize,
) -> FileChangeEvent {
    let diff_input_bytes = original
        .map(str::len)
        .unwrap_or_default()
        .saturating_add(updated.len());
    if diff_input_bytes > MAX_FILE_CHANGE_DIFF_INPUT_BYTES {
        let event_path = path_event_key(path);
        let change = match original {
            None => FileChange::Add {
                content: format!(
                    "<content omitted: input exceeded {MAX_FILE_CHANGE_DIFF_INPUT_BYTES} bytes>\n"
                ),
            },
            Some(_) => FileChange::Update {
                unified_diff: format!(
                    "@@\n-<diff omitted: input exceeded {MAX_FILE_CHANGE_DIFF_INPUT_BYTES} bytes>\n+<file updated successfully>\n"
                ),
                move_path: None,
            },
        };
        return FileChangeEvent {
            changes: HashMap::from([(event_path, change)]),
            delta: None,
        };
    }

    let event_path = path_event_key(path);
    let (change, committed_change) = match original {
        Some(original) => (
            FileChange::Update {
                unified_diff: TextDiff::from_lines(original, updated)
                    .unified_diff()
                    .context_radius(context_radius)
                    .to_string(),
                move_path: None,
            },
            AppliedPatchFileChange::Update {
                move_path: None,
                old_content: original.to_string(),
                overwritten_move_content: None,
                new_content: updated.to_string(),
            },
        ),
        None => (
            FileChange::Add {
                content: updated.to_string(),
            },
            AppliedPatchFileChange::Add {
                content: updated.to_string(),
                overwritten_content: None,
            },
        ),
    };
    let delta = AppliedPatchDelta::from_exact_changes(vec![AppliedPatchChange {
        path: path.clone(),
        change: committed_change,
    }]);
    FileChangeEvent {
        changes: HashMap::from([(event_path, change)]),
        delta: Some(delta),
    }
}

pub(super) fn path_event_key(path: &PathUri) -> PathBuf {
    path.to_abs_path()
        .map(AbsolutePathBuf::into_path_buf)
        .unwrap_or_else(|_| PathBuf::from(path.inferred_native_path_string()))
}
