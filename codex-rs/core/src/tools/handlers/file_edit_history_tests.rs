use super::*;
use pretty_assertions::assert_eq;

#[test]
fn restores_exact_content_from_numbered_read_output() {
    assert_eq!(
        content_from_read_output("1\talpha\n2\tbeta\n3\t"),
        Some("alpha\nbeta\n".to_string())
    );
}

#[test]
fn ignores_non_content_read_outputs() {
    assert_eq!(content_from_read_output(FILE_UNCHANGED_STUB), None);
    assert_eq!(
        content_from_read_output(
            "<system-reminder>Warning: the file exists but is shorter than the provided offset (4).</system-reminder>"
        ),
        None
    );
}
