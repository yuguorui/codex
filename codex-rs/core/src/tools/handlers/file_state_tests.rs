use super::*;
use pretty_assertions::assert_eq;

fn path(name: &str) -> PathUri {
    PathUri::parse(&format!("file:///tmp/{name}")).expect("test path")
}

fn read_state(
    content: &str,
    modified_at_ms: i64,
    offset: usize,
    limit: Option<usize>,
) -> ReadFileState {
    ReadFileState {
        content: content.to_string(),
        modified_at_ms,
        offset,
        limit,
    }
}

#[test]
fn read_state_allows_edit_while_mtime_is_unchanged() {
    let cache = FileStateCache::default();
    let path = path("range.txt");
    cache.record_read("local", &path, read_state("beta", 10, 2, Some(1)));

    assert_eq!(
        cache.validate_edit("local", &path, 10, "alpha\nbeta\ngamma"),
        Ok(())
    );
}

#[test]
fn read_state_rejects_changed_selected_content_with_the_same_mtime() {
    let cache = FileStateCache::default();
    let path = path("same-mtime.txt");
    cache.record_read("local", &path, read_state("beta", 10, 2, Some(1)));

    assert_eq!(
        cache.validate_edit("local", &path, 10, "alpha\nchanged\ngamma"),
        Err(EditStateError::Modified)
    );
}

#[test]
fn newer_mtime_rejects_a_read_state_even_when_content_matches() {
    let cache = FileStateCache::default();
    let path = path("read.txt");
    cache.record_read("local", &path, read_state("alpha", 10, 1, None));

    assert_eq!(
        cache.validate_edit("local", &path, 11, "alpha"),
        Err(EditStateError::Modified)
    );
}

#[test]
fn older_mtime_rejects_a_read_state_even_when_content_matches() {
    let cache = FileStateCache::default();
    let path = path("restored.txt");
    cache.record_read("local", &path, read_state("alpha", 10, 1, None));

    assert_eq!(
        cache.validate_edit("local", &path, 9, "alpha"),
        Err(EditStateError::Modified)
    );
}

#[test]
fn edit_state_uses_exact_content_for_any_mtime() {
    let cache = FileStateCache::default();
    let path = path("edit.txt");
    cache.record_edit(
        "local",
        &path,
        EditedFileState {
            content: "alpha".to_string(),
            modified_at_ms: 10,
        },
    );

    assert_eq!(cache.validate_edit("local", &path, 11, "alpha"), Ok(()));
    assert_eq!(cache.validate_edit("local", &path, 9, "alpha"), Ok(()));
    assert_eq!(
        cache.validate_edit("local", &path, 11, "changed"),
        Err(EditStateError::Modified)
    );
}

#[test]
fn unchanged_read_requires_the_same_range_and_mtime() {
    let cache = FileStateCache::default();
    let path = path("dedup.txt");
    cache.record_read("local", &path, read_state("beta", 10, 2, Some(1)));

    assert!(cache.read_is_unchanged("local", &path, 10, 2, Some(1), "beta"));
    assert!(!cache.read_is_unchanged("local", &path, 10, 1, Some(1), "beta"));
    assert!(!cache.read_is_unchanged("local", &path, 11, 2, Some(1), "beta"));
    assert!(!cache.read_is_unchanged("local", &path, 10, 2, Some(1), "changed"));
}

#[test]
fn history_rewrite_clears_cached_reads() {
    let cache = FileStateCache::default();
    let path = path("history.txt");
    cache.prepare_history_version(3);
    cache.record_read("local", &path, read_state("alpha", 10, 1, None));

    cache.prepare_history_version(4);

    assert_eq!(
        cache.validate_edit("local", &path, 10, "alpha"),
        Err(EditStateError::NotRead)
    );
}

#[test]
fn state_is_scoped_by_environment_and_path() {
    let cache = FileStateCache::default();
    let first = path("first.txt");
    let second = path("second.txt");
    cache.record_read("local", &first, read_state("alpha", 10, 1, None));

    assert_eq!(cache.validate_edit("local", &first, 10, "alpha"), Ok(()));
    assert_eq!(
        cache.validate_edit("remote", &first, 10, "alpha"),
        Err(EditStateError::NotRead)
    );
    assert_eq!(
        cache.validate_edit("local", &second, 10, "alpha"),
        Err(EditStateError::NotRead)
    );
}

#[test]
fn least_recently_used_entry_is_evicted_at_the_entry_limit() {
    let cache = FileStateCache::default();
    for index in 0..MAX_FILE_STATE_ENTRIES {
        cache.record_read(
            "local",
            &path(&format!("{index}.txt")),
            read_state(&index.to_string(), 10, 1, None),
        );
    }
    assert_eq!(
        cache.validate_edit("local", &path("0.txt"), 10, "0"),
        Ok(())
    );
    cache.record_read(
        "local",
        &path("extra.txt"),
        read_state("extra", 10, 1, None),
    );

    assert_eq!(
        cache.validate_edit("local", &path("1.txt"), 10, "1"),
        Err(EditStateError::NotRead)
    );
    assert_eq!(
        cache.validate_edit("local", &path("0.txt"), 10, "0"),
        Ok(())
    );
}

#[test]
fn normalization_removes_bom_and_crlf() {
    assert_eq!(
        normalize_file_content("\u{feff}alpha\r\nbeta\r\n"),
        "alpha\nbeta\n"
    );
}
