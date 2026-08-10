use super::*;
use codex_workflow::validate_workflow_script;

#[test]
fn bundled_workflows_pass_runtime_validation() {
    for name in ["code-review", "deep-research"] {
        let source = get(name).expect("bundled workflow should be registered");
        validate_workflow_script(source)
            .unwrap_or_else(|error| panic!("bundled workflow {name} is invalid: {error}"));
    }
}
