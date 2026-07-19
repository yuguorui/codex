use super::*;

#[test]
fn identifies_only_absolute_paths_for_history_restoration() {
    assert!(is_absolute_file_path("/tmp/file.txt"));
    assert!(is_absolute_file_path("C:\\workspace\\file.txt"));
    assert!(is_absolute_file_path("file:///workspace/file.txt"));
    assert!(!is_absolute_file_path("file.txt"));
    assert!(!is_absolute_file_path("subdir/file.txt"));
}

#[test]
fn preserves_trailing_spaces_in_file_names() {
    let cwd = PathUri::parse("file:///tmp").expect("cwd URI");

    let resolved =
        resolve_tool_path("Read", "file_path", &cwd, "/tmp/report ").expect("resolve path");

    assert_eq!(
        resolved,
        PathUri::parse("file:///tmp/report%20").expect("URI")
    );
}

#[test]
fn final_path_error_is_bounded_even_when_the_source_repeats_the_input() {
    let cwd = PathUri::parse("file:///tmp").expect("cwd URI");
    let path = format!("{}\0", "x".repeat(10_000));
    let error =
        resolve_tool_path("Read", "file_path", &cwd, &path).expect_err("invalid path should fail");
    let FunctionCallError::RespondToModel(message) = error else {
        panic!("expected model-facing error");
    };

    assert!(message.len() < 3_000);
    assert!(message.matches("chars truncated").count() >= 1);
    assert!(!message.contains(&path));
}

#[test]
fn binary_extension_filter_matches_claude_code_file_types() {
    assert_eq!(blocked_binary_extension("/tmp/archive.ZIP"), Some(".zip"));
    assert_eq!(
        blocked_binary_extension(r"C:\data\records.sqlite"),
        Some(".sqlite")
    );
    assert_eq!(blocked_binary_extension("/tmp/image.png"), None);
    assert_eq!(blocked_binary_extension("/tmp/document.pdf"), None);
    assert_eq!(blocked_binary_extension("/tmp/source.rs"), None);
}
