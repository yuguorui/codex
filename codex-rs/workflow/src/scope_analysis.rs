use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
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

/// Finds only free references to APIs that the workflow runtime explicitly removes.
///
/// Parser or semantic errors deliberately produce no finding. The V8 runtime remains
/// authoritative for JavaScript validity, while this check favors false negatives over
/// rejecting a script that the runtime could execute.
pub(crate) fn find_unavailable_global(body: &str) -> Option<UnavailableGlobal> {
    if DYNAMIC_SCOPE_MARKERS
        .iter()
        .any(|marker| body.contains(marker))
    {
        return None;
    }

    let source = format!("{ANALYSIS_PREFIX}{body}{ANALYSIS_SUFFIX}");
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &source, SourceType::default()).parse();
    if !parsed.diagnostics.is_empty() {
        return None;
    }

    let semantic = SemanticBuilder::new_compiler()
        .with_build_nodes(true)
        .build(&parsed.program);
    if !semantic.diagnostics.is_empty() {
        return None;
    }

    let semantic = semantic.semantic;
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
