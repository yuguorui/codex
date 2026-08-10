use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn empty_declarations_require_no_execution_environment() {
    assert_eq!(
        freeze_declared_inputs(&[], &[]).await,
        Ok(WorkflowDeclaredInputs::default())
    );
}

#[test]
fn declarations_reject_paths_outside_the_workspace() {
    for pattern in [
        "/tmp/input.txt",
        "../input.txt",
        "C:/input.txt",
        "src\\input.txt",
    ] {
        let Err(error) = declared_pattern(pattern, false) else {
            panic!("unsafe pattern must be rejected");
        };
        assert!(error.contains("must be workspace-relative"));
    }
}

#[tokio::test]
async fn declarations_enforce_the_pattern_count_limit_before_filesystem_access() {
    let patterns = (0..=MAX_INPUT_PATTERNS)
        .map(|index| format!("input-{index}.txt"))
        .collect::<Vec<_>>();

    assert_eq!(
        freeze_declared_inputs(&patterns, &[]).await,
        Err(format!(
            "meta.inputs supports at most {MAX_INPUT_PATTERNS} patterns"
        ))
    );
}
