use super::*;
use pretty_assertions::assert_eq;

fn request<'a>(
    old_string: &'a str,
    new_string: &'a str,
    replace_all: bool,
) -> EditContentRequest<'a> {
    EditContentRequest {
        old_string,
        new_string,
        replace_all,
    }
}

#[test]
fn unique_edit_replaces_one_match() {
    let edit = prepare_edit(request("beta", "BETA", false), Some("alpha\nbeta\ngamma\n"))
        .expect("prepare edit");

    assert_eq!(edit.updated_content, "alpha\nBETA\ngamma\n");
}

#[test]
fn ambiguous_edit_requires_replace_all() {
    let error = prepare_edit(request("same", "new", false), Some("same\nsame\n"))
        .err()
        .expect("ambiguous edit should fail");

    assert_eq!(
        error,
        FunctionCallError::RespondToModel(
            "Found 2 matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: same".to_string()
        )
    );
}

#[test]
fn replace_all_replaces_every_match() {
    let edit = prepare_edit(request("same", "new", true), Some("same\nsame\n"))
        .expect("prepare replace all");

    assert_eq!(edit.updated_content, "new\nnew\n");
}

#[test]
fn deleting_a_line_consumes_its_trailing_newline() {
    let edit = prepare_edit(request("beta", "", false), Some("alpha\nbeta\ngamma\n"))
        .expect("prepare deletion");

    assert_eq!(edit.updated_content, "alpha\ngamma\n");
}

#[test]
fn empty_old_string_creates_new_file_or_replaces_empty_file() {
    let new_file = prepare_edit(request("", "content\n", false), None).expect("create file");
    let empty_file =
        prepare_edit(request("", "content\n", false), Some(" \n")).expect("replace empty file");
    let bom_only_file = prepare_edit(request("", "content\n", false), Some("\u{feff}"))
        .expect("replace BOM-only file");

    assert_eq!(new_file.updated_content, "content\n");
    assert_eq!(empty_file.updated_content, "content\n");
    assert_eq!(bom_only_file.updated_content, "content\n");
}

#[test]
fn normalized_quote_match_preserves_curly_quote_style() {
    let edit = prepare_edit(
        request("The \"old\" value", "The \"new\" value", false),
        Some("The “old” value\n"),
    )
    .expect("prepare quote-normalized edit");

    assert_eq!(edit.updated_content, "The “new” value\n");
}

#[test]
fn normalized_replacement_that_changes_nothing_is_rejected() {
    let error = prepare_edit(request("'same'", "‘same’", false), Some("‘same’\n"))
        .err()
        .expect("unchanged normalized edit should fail");

    assert_eq!(
        error,
        FunctionCallError::RespondToModel(
            "No changes to make: the requested replacement leaves the file unchanged.".to_string()
        )
    );
}

#[test]
fn missing_match_error_bounds_model_supplied_text() {
    let search = "x".repeat(10_000);
    let error = prepare_edit(request(&search, "new", false), Some("different\n"))
        .err()
        .expect("missing match should fail");
    let FunctionCallError::RespondToModel(message) = error else {
        panic!("expected model-facing error");
    };

    assert!(message.len() < 2_000);
    assert!(message.contains("chars truncated"));
}

#[test]
fn expanded_replacement_is_rejected_before_allocation() {
    assert_eq!(
        validate_updated_size(
            MAX_EDIT_FILE_SIZE as usize,
            1,
            2,
            MAX_EDIT_FILE_SIZE as usize,
        ),
        Err(FunctionCallError::RespondToModel(format!(
            "The edited file would exceed the maximum editable file size of {MAX_EDIT_FILE_SIZE} bytes. Use smaller replacements or another editing strategy."
        )))
    );
}

#[test]
fn multiline_edit_preserves_crlf_line_endings() {
    let original = "alpha\r\nbeta\r\ngamma\r\n";
    let edit = prepare_edit(request("alpha\nbeta", "ALPHA\nBETA", false), Some(original))
        .expect("prepare CRLF edit");

    assert_eq!(edit.updated_content, "ALPHA\r\nBETA\r\ngamma\r\n");
}

#[test]
fn edit_preserves_missing_final_newline() {
    let edit = prepare_edit(request("beta", "BETA", false), Some("alpha\nbeta"))
        .expect("prepare edit without final newline");

    assert_eq!(edit.updated_content, "alpha\nBETA");
}
