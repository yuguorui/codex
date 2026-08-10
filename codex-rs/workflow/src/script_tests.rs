use pretty_assertions::assert_eq;

use super::*;
use crate::WorkflowPhase;
use crate::scope_analysis::UNAVAILABLE_GLOBAL_NAMES;

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

#[test]
fn rejects_free_references_to_unavailable_outer_apis_with_actionable_guidance() {
    let error = validate_workflow_script(valid_script(
        "const synthesis = await agent('summarize');\ntext(JSON.stringify(synthesis));",
    ))
    .unwrap_err();

    assert_eq!(
        error,
        WorkflowScriptError::UnavailableGlobal {
            name: "text".to_string(),
            line: 3,
            column: 1,
            guidance: "Finish with the final value, for example `return { synthesis };`.",
        }
    );
}

#[test]
fn unavailable_global_location_uses_full_workflow_source_coordinates() {
    let error = validate_workflow_script(
        "export const meta = {\n  name: 'test',\n  description: 'desc',\n};\nreturn text(args)",
    )
    .unwrap_err();

    assert!(matches!(
        error,
        WorkflowScriptError::UnavailableGlobal {
            name,
            line: 5,
            column: 8,
            ..
        } if name == "text"
    ));
}

#[test]
fn accepts_lexically_bound_names_that_match_unavailable_outer_apis() {
    for body in [
        "const text = value => value; return text(args)",
        "function finish(text) { return text } return finish(args)",
        "const { text } = args; return () => text",
        "try { throw args } catch (tools) { return tools }",
        "return (({ notify: exit }) => exit)(args)",
    ] {
        validate_workflow_script(valid_script(body)).unwrap_or_else(|error| {
            panic!("rejected lexically bound unavailable API name in `{body}`: {error}")
        });
    }
}

#[test]
fn checks_every_runtime_removed_global_without_rejecting_local_bindings() {
    for name in UNAVAILABLE_GLOBAL_NAMES {
        let error =
            validate_workflow_script(valid_script(&format!("return {name}(args)"))).unwrap_err();
        assert!(matches!(
            error,
            WorkflowScriptError::UnavailableGlobal {
                name: unavailable,
                ..
            } if unavailable == *name
        ));

        validate_workflow_script(valid_script(&format!(
            "const {name} = value => value; return {name}(args)"
        )))
        .unwrap_or_else(|error| panic!("rejected local `{name}` binding: {error}"));
    }
}

#[test]
fn allows_other_unknown_globals_to_avoid_false_positive_typo_guesses() {
    validate_workflow_script(valid_script("return projectProvidedGlobal(args)"))
        .expect("unknown globals are intentionally outside the conservative check");
}

#[test]
fn skips_findings_when_a_free_binding_could_be_established_dynamically() {
    for body in [
        "globalThis.text = value => value; return text(args)",
        "text = value => value; return text(args)",
        "setupProjectGlobals(); return text(args)",
        "with ({ text: value => value }) { return text(args) }",
        "eval('const text = value => value'); return text(args)",
    ] {
        validate_workflow_script(valid_script(body)).unwrap_or_else(|error| {
            panic!("rejected dynamically resolvable unavailable API in `{body}`: {error}")
        });
    }
}

#[test]
fn accepts_non_throwing_typeof_checks_for_unavailable_globals() {
    validate_workflow_script(valid_script("return typeof text"))
        .expect("typeof on an unavailable global evaluates to undefined");
}

#[test]
fn skips_unavailable_globals_in_conditional_or_nested_execution_contexts() {
    for body in [
        "if (false) text(args)",
        "false && text(args)",
        "try { text(args) } catch {}",
        "const callback = () => text(args); return callback",
        "switch (args) { case 1: text(args) }",
    ] {
        validate_workflow_script(valid_script(body)).unwrap_or_else(|error| {
            panic!("rejected conditionally evaluated unavailable API in `{body}`: {error}")
        });
    }
}

#[test]
fn defers_javascript_syntax_errors_to_v8() {
    validate_workflow_script(valid_script("text(]"))
        .expect("the static lint must not override the V8 parser");
}

#[test]
fn rejects_statically_invalid_agent_prompts_before_starting_the_workflow() {
    for (body, reason) in [
        ("return agent()", "the prompt argument is missing"),
        ("return agent('  ')", "the prompt string is empty"),
        ("return agent('\\u00a0')", "the prompt string is empty"),
        ("return agent(`\n\t`)", "the prompt template is empty"),
        (
            "return agent(['review this', 'carefully'])",
            "the prompt is statically not a string",
        ),
        (
            "return agent({ prompt: 'work' })",
            "the prompt is statically not a string",
        ),
    ] {
        assert!(matches!(
            validate_workflow_script(valid_script(body)),
            Err(WorkflowScriptError::InvalidAgentPrompt {
                reason: actual_reason,
                ..
            }) if actual_reason == reason
        ));
    }
}

#[test]
fn allows_dynamic_agent_prompts_and_shadowed_agent_bindings() {
    for body in [
        "return agent(args.prompt)",
        "return agent(['one', 'two'].join(' '))",
        "return agent(`inspect ${args.target}`)",
        "return agent('\\u0085')",
        "function nested(agent) { return agent([]) } return nested(value => value)",
        "with ({ agent: value => value }) { return agent([]) }",
    ] {
        validate_workflow_script(valid_script(body))
            .unwrap_or_else(|error| panic!("rejected dynamic prompt in `{body}`: {error}"));
    }
}
