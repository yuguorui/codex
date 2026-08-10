use pretty_assertions::assert_eq;

use super::*;
use crate::WorkflowPhase;

fn valid_script(body: &str) -> String {
    format!(
        "export const meta = {{ name: 'test', description: 'desc', phases: [{{ title: `Run` }}], }};\n{body}"
    )
}

#[test]
fn parses_json5_metadata_and_returns_the_body() {
    let script = validate_workflow_script(valid_script("return args")).unwrap();

    assert_eq!(
        script.meta,
        WorkflowMeta {
            name: "test".to_string(),
            description: "desc".to_string(),
            title: None,
            when_to_use: None,
            phases: vec![WorkflowPhase {
                title: "Run".to_string(),
                detail: None,
                model: None,
            }],
        }
    );
    assert_eq!(script.body.trim(), "return args");
}

#[test]
fn rejects_nonliteral_metadata() {
    for source in [
        "export const meta = makeMeta();",
        "export const meta = { name, description: 'desc' };",
        "export const meta = { ...base, name: 'x', description: 'desc' };",
        "export const meta = { name: `x${value}`, description: 'desc' };",
    ] {
        assert!(
            validate_workflow_script(source).is_err(),
            "accepted {source}"
        );
    }
}

#[test]
fn requires_metadata_to_be_the_first_statement() {
    let error = validate_workflow_script("const x = 1; export const meta = {};").unwrap_err();

    assert_eq!(error, WorkflowScriptError::MissingMeta);
}

#[test]
fn rejects_disallowed_control_characters_and_large_scripts() {
    assert!(matches!(
        validate_workflow_script(valid_script("log('\u{0}')")),
        Err(WorkflowScriptError::ControlCharacter(_))
    ));
    assert_eq!(
        validate_workflow_script("x".repeat(MAX_WORKFLOW_SCRIPT_BYTES + 1)).unwrap_err(),
        WorkflowScriptError::TooLarge
    );
}

#[test]
fn rejects_phase_titles_that_exceed_the_progress_text_limit() {
    let title = "x".repeat(MAX_WORKFLOW_PROGRESS_TEXT_BYTES + 1);
    let source = format!(
        "export const meta = {{ name: 'test', description: 'desc', phases: [{{ title: {title:?} }}] }}; return null"
    );

    assert_eq!(
        validate_workflow_script(source).unwrap_err(),
        WorkflowScriptError::MetaFieldTooLarge {
            field: "phases[].title",
            max_bytes: MAX_WORKFLOW_PROGRESS_TEXT_BYTES,
        }
    );
}

#[test]
fn rejects_metadata_that_cannot_safely_reach_paths_or_terminal_ui() {
    let oversized_name = "n".repeat(129);
    let name_error = validate_workflow_script(format!(
        "export const meta = {{ name: {oversized_name:?}, description: 'test' }}; return null"
    ))
    .unwrap_err();
    assert_eq!(
        name_error,
        WorkflowScriptError::MetaFieldTooLarge {
            field: "name",
            max_bytes: 128,
        }
    );

    let oversized_title = "t".repeat(257);
    let title_error = validate_workflow_script(format!(
        "export const meta = {{ name: 'test', description: 'test', title: {oversized_title:?} }}; return null"
    ))
    .unwrap_err();
    assert_eq!(
        title_error,
        WorkflowScriptError::MetaFieldTooLarge {
            field: "title",
            max_bytes: 256,
        }
    );
}

#[test]
fn rejects_direct_nondeterministic_apis_but_not_strings_or_comments() {
    for (body, api) in [
        ("return Date.now()", "Date.now()"),
        ("return Math . random ()", "Math.random()"),
        ("return new /* clock */ Date()", "new Date()"),
    ] {
        assert_eq!(
            validate_workflow_script(valid_script(body)).unwrap_err(),
            WorkflowScriptError::Nondeterministic(api)
        );
    }
    validate_workflow_script(valid_script(
        "// Date.now()\nreturn 'Math.random() and new Date()'",
    ))
    .unwrap();
    validate_workflow_script(valid_script(
        r"return /Date\.now\(\)|Math\.random\(\)/.test(args)",
    ))
    .unwrap();
    validate_workflow_script(valid_script("return new Date(0).toISOString()"))
        .expect("a Date constructed from an explicit value is deterministic");
}

#[test]
fn rejects_reserved_runtime_identifiers_including_template_expressions() {
    for body in ["return __wfHostAgent", "return `${__wfHostAgent}`"] {
        assert!(matches!(
            validate_workflow_script(valid_script(body)),
            Err(WorkflowScriptError::ReservedIdentifier(_))
        ));
    }

    validate_workflow_script(valid_script("return '__wfHostAgent'"))
        .expect("quoted internal-looking text is inert");
}

#[test]
fn rejects_unicode_escapes_that_can_reconstruct_internal_identifiers() {
    for body in [
        r"return \u005f\u005fwfOriginalDate.now()",
        r"return 1 / \u005f\u005fwfOriginalDate / 2",
    ] {
        assert!(matches!(
            validate_workflow_script(valid_script(body)),
            Err(WorkflowScriptError::EscapedIdentifier(_))
        ));
    }

    validate_workflow_script(valid_script(
        r"return /[\u200b-\u200f]__wfHostAgent/g.test(args)",
    ))
    .expect("regex contents cannot reference workflow runtime bindings");
}
