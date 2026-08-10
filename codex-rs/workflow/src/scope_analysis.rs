use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_ast::ast::Expression;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::GetSpan;
use oxc_span::SourceType;

pub(crate) const UNAVAILABLE_GLOBAL_NAMES: &[&str] = &[
    "tools",
    "ALL_TOOLS",
    "text",
    "image",
    "audio",
    "generatedImage",
    "store",
    "load",
    "notify",
    "yield_control",
    "exit",
];

const ANALYSIS_PREFIX: &str = r#"
const args = undefined;
const agent = undefined;
const parallel = undefined;
const pipeline = undefined;
const phase = undefined;
const log = undefined;
const budget = undefined;
const workflow = undefined;
const console = undefined;
const __wfMain = async () => {
"#;
const ANALYSIS_SUFFIX: &str = "\n};\n";

// Any of these can make a free identifier resolve dynamically. Substring matching is
// intentionally broad: skipping analysis unnecessarily is preferable to rejecting valid code.
const DYNAMIC_SCOPE_MARKERS: &[&str] = &[
    "globalThis",
    "eval",
    "Function",
    "constructor",
    "delete",
    "this",
    "typeof",
    "with",
];

// These are intrinsic ECMAScript globals rather than host-provided extension points. Unknown
// globals outside this list make the analysis bail out because calling one could establish a
// previously unavailable binding before it is read.
const ECMASCRIPT_GLOBAL_NAMES: &[&str] = &[
    "AggregateError",
    "Array",
    "ArrayBuffer",
    "Atomics",
    "BigInt",
    "BigInt64Array",
    "BigUint64Array",
    "Boolean",
    "DataView",
    "Date",
    "Error",
    "EvalError",
    "FinalizationRegistry",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "Infinity",
    "Int8Array",
    "Int16Array",
    "Int32Array",
    "Intl",
    "Iterator",
    "JSON",
    "Map",
    "Math",
    "NaN",
    "Number",
    "Object",
    "Promise",
    "Proxy",
    "RangeError",
    "ReferenceError",
    "Reflect",
    "RegExp",
    "Set",
    "SharedArrayBuffer",
    "String",
    "Symbol",
    "SyntaxError",
    "Temporal",
    "TypeError",
    "URIError",
    "Uint8Array",
    "Uint8ClampedArray",
    "Uint16Array",
    "Uint32Array",
    "WeakMap",
    "WeakRef",
    "WeakSet",
    "WebAssembly",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
    "escape",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "undefined",
    "unescape",
];

pub(crate) struct UnavailableGlobal {
    pub(crate) name: String,
    pub(crate) byte_offset: usize,
    pub(crate) guidance: &'static str,
}

pub(crate) struct InvalidAgentPrompt {
    pub(crate) byte_offset: usize,
    pub(crate) reason: &'static str,
}

pub(crate) struct WorkflowBodyAnalysis {
    pub(crate) unavailable_global: Option<UnavailableGlobal>,
}

pub(crate) fn analyze_workflow_body(
    body: &str,
) -> Result<WorkflowBodyAnalysis, InvalidAgentPrompt> {
    let source = format!("{ANALYSIS_PREFIX}{body}{ANALYSIS_SUFFIX}");
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &source, SourceType::default()).parse();
    if !parsed.diagnostics.is_empty() {
        return Ok(WorkflowBodyAnalysis {
            unavailable_global: None,
        });
    }

    let semantic = SemanticBuilder::new_compiler()
        .with_build_nodes(true)
        .build(&parsed.program);
    if !semantic.diagnostics.is_empty() {
        return Ok(WorkflowBodyAnalysis {
            unavailable_global: None,
        });
    }

    let semantic = semantic.semantic;
    let has_dynamic_scope = DYNAMIC_SCOPE_MARKERS
        .iter()
        .any(|marker| body.contains(marker));
    if let Some(invalid_prompt) = find_invalid_agent_prompt(&semantic) {
        return Err(invalid_prompt);
    }
    let unavailable_global = if has_dynamic_scope {
        None
    } else {
        find_unavailable_global(&semantic, body)
    };
    Ok(WorkflowBodyAnalysis { unavailable_global })
}

fn find_invalid_agent_prompt(semantic: &oxc_semantic::Semantic<'_>) -> Option<InvalidAgentPrompt> {
    let scoping = semantic.scoping();
    semantic.nodes().iter().find_map(|node| {
        let AstKind::CallExpression(call) = node.kind() else {
            return None;
        };
        let Expression::Identifier(callee) = call.callee.without_parentheses() else {
            return None;
        };
        if callee.name != "agent" {
            return None;
        }
        let reference_id = callee.reference_id.get()?;
        let reference = scoping.get_reference(reference_id);
        if semantic
            .nodes()
            .ancestor_kinds(reference.node_id())
            .any(|ancestor| matches!(ancestor, AstKind::WithStatement(_)))
        {
            return None;
        }
        let symbol_id = reference.symbol_id()?;
        if usize::try_from(scoping.symbol_span(symbol_id).start).ok()? >= ANALYSIS_PREFIX.len() {
            return None;
        }

        let Some(argument) = call.arguments.first() else {
            return Some(InvalidAgentPrompt {
                byte_offset: body_offset(call.span.start),
                reason: "the prompt argument is missing",
            });
        };
        let expression = argument.as_expression()?;
        invalid_prompt_reason(expression).map(|reason| InvalidAgentPrompt {
            byte_offset: body_offset(expression.span().start),
            reason,
        })
    })
}

fn invalid_prompt_reason(expression: &Expression<'_>) -> Option<&'static str> {
    match expression.without_parentheses() {
        Expression::StringLiteral(literal) if is_empty_workflow_prompt(&literal.value) => {
            Some("the prompt string is empty")
        }
        Expression::TemplateLiteral(template)
            if template.expressions.is_empty()
                && template.quasis.iter().all(|quasi| {
                    quasi
                        .value
                        .cooked
                        .as_ref()
                        .is_some_and(|value| is_empty_workflow_prompt(value))
                }) =>
        {
            Some("the prompt template is empty")
        }
        Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_)
        | Expression::NumericLiteral(_)
        | Expression::BigIntLiteral(_)
        | Expression::RegExpLiteral(_)
        | Expression::ArrayExpression(_)
        | Expression::ArrowFunctionExpression(_)
        | Expression::ClassExpression(_)
        | Expression::FunctionExpression(_)
        | Expression::NewExpression(_)
        | Expression::ObjectExpression(_)
        | Expression::JSXElement(_)
        | Expression::JSXFragment(_) => Some("the prompt is statically not a string"),
        _ => None,
    }
}

fn is_empty_workflow_prompt(value: &str) -> bool {
    value.chars().all(|character| {
        matches!(
            character,
            '\u{0009}'
                | '\u{000A}'
                | '\u{000B}'
                | '\u{000C}'
                | '\u{000D}'
                | '\u{0020}'
                | '\u{00A0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200A}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202F}'
                    | '\u{205F}'
                    | '\u{3000}'
                    | '\u{FEFF}'
        )
    })
}

fn body_offset(source_offset: u32) -> usize {
    usize::try_from(source_offset)
        .unwrap_or_default()
        .saturating_sub(ANALYSIS_PREFIX.len())
}

fn find_unavailable_global(
    semantic: &oxc_semantic::Semantic<'_>,
    body: &str,
) -> Option<UnavailableGlobal> {
    let scoping = semantic.scoping();
    if scoping.root_unresolved_references().keys().any(|name| {
        !UNAVAILABLE_GLOBAL_NAMES.contains(&name.as_str())
            && !ECMASCRIPT_GLOBAL_NAMES.contains(&name.as_str())
    }) {
        return None;
    }
    UNAVAILABLE_GLOBAL_NAMES
        .iter()
        .filter_map(|name| {
            let references = scoping.root_unresolved_references().get(*name)?;
            if references
                .iter()
                .any(|reference_id| scoping.get_reference(*reference_id).is_write())
            {
                return None;
            }
            references
                .iter()
                .filter_map(|reference_id| {
                    let reference = scoping.get_reference(*reference_id);
                    let mut reached_workflow_body = false;
                    for ancestor in semantic.nodes().ancestor_kinds(reference.node_id()) {
                        match ancestor {
                            AstKind::ArrowFunctionExpression(arrow)
                                if usize::try_from(arrow.span.start)
                                    .is_ok_and(|start| start < ANALYSIS_PREFIX.len()) =>
                            {
                                reached_workflow_body = true;
                                break;
                            }
                            AstKind::ArrowFunctionExpression(_)
                            | AstKind::Function(_)
                            | AstKind::Class(_)
                            | AstKind::StaticBlock(_)
                            | AstKind::LogicalExpression(_)
                            | AstKind::ConditionalExpression(_)
                            | AstKind::AssignmentPattern(_)
                            | AstKind::IfStatement(_)
                            | AstKind::DoWhileStatement(_)
                            | AstKind::WhileStatement(_)
                            | AstKind::ForStatement(_)
                            | AstKind::ForInStatement(_)
                            | AstKind::ForOfStatement(_)
                            | AstKind::SwitchStatement(_)
                            | AstKind::SwitchCase(_)
                            | AstKind::LabeledStatement(_)
                            | AstKind::TryStatement(_)
                            | AstKind::CatchClause(_) => return None,
                            AstKind::Program(_) => break,
                            _ => {}
                        }
                    }
                    if !reached_workflow_body {
                        return None;
                    }
                    let span = semantic.reference_span(reference);
                    let body_offset = usize::try_from(span.start)
                        .ok()?
                        .checked_sub(ANALYSIS_PREFIX.len())?;
                    body.get(body_offset..)?;
                    Some((span.start, *name, body_offset))
                })
                .min_by_key(|(start, _, _)| *start)
        })
        .min_by_key(|(start, _, _)| *start)
        .map(|(_, name, byte_offset)| UnavailableGlobal {
            name: name.to_string(),
            byte_offset,
            guidance: guidance(name),
        })
}

fn guidance(name: &str) -> &'static str {
    match name {
        "text" => "Finish with the final value, for example `return { synthesis };`.",
        "tools" | "ALL_TOOLS" => {
            "Call a Workflow API directly: `agent`, `parallel`, `pipeline`, or `workflow`."
        }
        "store" | "load" => {
            "Keep state in local variables and use values returned by `agent(...)` or `workflow(...)`."
        }
        "notify" | "yield_control" => {
            "Use `log(...)` for progress; Workflow scheduling and progress delivery are managed automatically."
        }
        "exit" => "Return a value from the Workflow to finish it.",
        "image" | "audio" | "generatedImage" => {
            "Return a JSON-compatible result from the Workflow."
        }
        _ => "Declare a local binding or use an API exposed inside the Workflow.",
    }
}
