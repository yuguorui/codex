use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_runtime::V8JitMode;
use codex_code_mode_runtime::initialize_v8;
use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExposure;
use codex_tools::ToolSpec;
use codex_tools::parse_tool_input_schema;
use codex_workflow::ResolvedWorkflowInputs;
use codex_workflow::WorkflowAgentInputs;
use codex_workflow::WorkflowInputArtifactKind;
use codex_workflow::WorkflowInputArtifactRef;
use codex_workflow::WorkflowInputDescriptor;
use codex_workflow::WorkflowInputPathSegment;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::sync::OnceCell;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

pub(crate) const ANALYZE_WORKFLOW_INPUTS_TOOL_NAME: &str = "AnalyzeWorkflowInputs";
const MAX_ANALYSIS_LOG_BYTES: usize = 1024;
const MAX_ANALYSIS_LOG_ARGUMENTS: i32 = 32;
const MAX_ANALYSIS_OUTPUT_BYTES: usize = 960;
const MAX_HELPER_SLICE_BYTES: usize = 64 * 1024;
const MAX_MODEL_ERROR_BYTES: usize = 384;
const MAX_CONCURRENT_ANALYSIS_ISOLATES: usize = 4;
const MAX_CONCURRENT_INPUT_MATERIALIZATIONS: usize = 4;
const ANALYSIS_HEAP_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const ANALYSIS_HEAP_EMERGENCY_BYTES: usize = 8 * 1024 * 1024;
const ANALYSIS_ISOLATE_INIT_TIMEOUT: Duration = Duration::from_secs(2);
const ANALYSIS_SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const ANALYSIS_CPU_TIMEOUT: Duration = Duration::from_millis(750);
const ANALYSIS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(2);
const INPUT_RECURSION_GUARD: usize = 256;
static ANALYSIS_ISOLATE_ADMISSION: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_ANALYSIS_ISOLATES)));
static ANALYSIS_MATERIALIZATION_ADMISSION: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_INPUT_MATERIALIZATIONS)));

pub(crate) struct WorkflowInputsCapability {
    inputs: WorkflowAgentInputs,
    resolved: OnceCell<Arc<ResolvedWorkflowInputs>>,
}

impl WorkflowInputsCapability {
    pub(crate) fn new(inputs: WorkflowAgentInputs) -> Self {
        Self {
            inputs,
            resolved: OnceCell::new(),
        }
    }

    async fn resolve(&self) -> Result<Arc<ResolvedWorkflowInputs>, String> {
        self.resolved
            .get_or_try_init(|| async {
                let _permit = Arc::clone(&ANALYSIS_MATERIALIZATION_ADMISSION)
                    .acquire_owned()
                    .await
                    .map_err(|_| "workflow input materialization is unavailable".to_string())?;
                self.inputs.resolve_shared().await.map(Arc::new)
            })
            .await
            .map(Arc::clone)
    }
}

fn model_error(message: impl std::fmt::Display) -> FunctionCallError {
    let message = bounded_text(&message.to_string(), MAX_MODEL_ERROR_BYTES);
    FunctionCallError::RespondToModel(message)
}

pub(crate) struct AnalyzeWorkflowInputsToolExecutor {
    capability: Arc<WorkflowInputsCapability>,
}

impl AnalyzeWorkflowInputsToolExecutor {
    pub(crate) fn new(capability: Arc<WorkflowInputsCapability>) -> Self {
        Self { capability }
    }
}

impl ToolExecutor<ToolCall> for AnalyzeWorkflowInputsToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(ANALYZE_WORKFLOW_INPUTS_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        analyze_workflow_inputs_spec()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let arguments = invocation.function_arguments().map_err(model_error)?;
            let args: AnalyzeWorkflowInputsArgs =
                serde_json::from_str(arguments).map_err(|error| {
                    model_error(format!(
                        "invalid {ANALYZE_WORKFLOW_INPUTS_TOOL_NAME} input: {error}"
                    ))
                })?;
            let item = ExtensionTurnItem::workflow_input_analysis(invocation.call_id.clone());
            invocation
                .turn_item_emitter
                .emit_started(item.clone())
                .await;
            let output = analyze_capability_inputs(&self.capability, args.program).await;
            invocation.turn_item_emitter.emit_completed(item).await;
            let output = output.map_err(model_error)?;
            let value = json!({
                "result": output.result,
                "logs": output.logs,
                "logsTruncated": output.logs_truncated,
            });
            let serialized = serde_json::to_vec(&value).map_err(|error| {
                model_error(format!(
                    "failed to serialize workflow input analysis: {error}"
                ))
            })?;
            if serialized.len() > MAX_ANALYSIS_OUTPUT_BYTES {
                return Err(model_error(format!(
                    "{ANALYZE_WORKFLOW_INPUTS_TOOL_NAME} should return a focused view; inspect the remaining inputs with additional calls"
                )));
            }
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnalyzeWorkflowInputsArgs {
    program: String,
}

#[derive(Debug)]
struct AnalysisOutput {
    result: JsonValue,
    logs: Vec<String>,
    logs_truncated: bool,
}

#[cfg(test)]
async fn analyze_inputs(inputs: Arc<JsonValue>, program: String) -> Result<AnalysisOutput, String> {
    validate_analysis_program(&program)?;
    let inputs = shared_analysis_inputs(inputs)?;
    let permit = analysis_permit().await?;
    analyze_inputs_with_permit(inputs, program, permit).await
}

async fn analyze_capability_inputs(
    capability: &WorkflowInputsCapability,
    program: String,
) -> Result<AnalysisOutput, String> {
    validate_analysis_program(&program)?;
    let inputs = capability.resolve().await?;
    let permit = analysis_permit().await?;
    analyze_inputs_with_permit(inputs, program, permit).await
}

#[cfg(test)]
fn shared_analysis_inputs(inputs: Arc<JsonValue>) -> Result<Arc<ResolvedWorkflowInputs>, String> {
    let JsonValue::Object(inputs) = inputs.as_ref() else {
        return Err("provide workflow analysis inputs as an object of named values".to_string());
    };
    ResolvedWorkflowInputs::from_values(
        inputs
            .iter()
            .map(|(alias, value)| (alias.clone(), Arc::new(value.clone())))
            .collect(),
    )
    .map(Arc::new)
}

fn validate_analysis_program(program: &str) -> Result<(), String> {
    if program.trim().is_empty() {
        return Err("provide a non-empty analysis program".to_string());
    }
    Ok(())
}

async fn analysis_permit() -> Result<OwnedSemaphorePermit, String> {
    Arc::clone(&ANALYSIS_ISOLATE_ADMISSION)
        .acquire_owned()
        .await
        .map_err(|_| "workflow input analysis is unavailable".to_string())
}

async fn analyze_inputs_with_permit(
    inputs: Arc<ResolvedWorkflowInputs>,
    program: String,
    permit: OwnedSemaphorePermit,
) -> Result<AnalysisOutput, String> {
    let (isolate_tx, isolate_rx) = tokio::sync::oneshot::channel();
    let (execution_guard_ready_tx, execution_guard_ready_rx) = tokio::sync::oneshot::channel();
    let (program_ready_tx, program_ready_rx) = tokio::sync::oneshot::channel();
    let mut task = tokio::task::spawn_blocking(move || {
        run_analysis_isolate(
            inputs,
            &program,
            isolate_tx,
            execution_guard_ready_rx,
            program_ready_tx,
            permit,
        )
    });
    let isolate_handle = match tokio::time::timeout(ANALYSIS_ISOLATE_INIT_TIMEOUT, isolate_rx).await
    {
        Ok(Ok(isolate_handle)) => isolate_handle,
        Ok(Err(_)) => {
            return task
                .await
                .map_err(|error| format!("analysis isolate failed: {error}"))?;
        }
        Err(_) => return Err("analysis isolate did not initialize in time".to_string()),
    };
    let mut execution_guard = IsolateExecutionGuard::new(isolate_handle);
    execution_guard_ready_tx
        .send(())
        .map_err(|_| "analysis isolate ended before execution guard setup".to_string())?;

    let setup = tokio::time::timeout(ANALYSIS_SETUP_TIMEOUT, program_ready_rx);
    tokio::pin!(setup);
    tokio::select! {
        result = &mut task => {
            execution_guard.disarm();
            return result.map_err(|error| format!("analysis isolate failed: {error}"))?;
        }
        ready = &mut setup => {
            match ready {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    let result = task.await;
                    execution_guard.disarm();
                    return result
                        .map_err(|error| format!("analysis isolate failed: {error}"))?;
                }
                Err(_) => {
                    execution_guard.terminate();
                    if tokio::time::timeout(ANALYSIS_TERMINATION_TIMEOUT, &mut task)
                        .await
                        .is_ok()
                    {
                        execution_guard.disarm();
                    }
                    return Err("analysis inputs took too long to prepare".to_string());
                }
            }
        }
    }

    match tokio::time::timeout(ANALYSIS_CPU_TIMEOUT, &mut task).await {
        Ok(result) => {
            execution_guard.disarm();
            result.map_err(|error| format!("analysis isolate failed: {error}"))?
        }
        Err(_) => {
            execution_guard.terminate();
            if tokio::time::timeout(ANALYSIS_TERMINATION_TIMEOUT, &mut task)
                .await
                .is_ok()
            {
                execution_guard.disarm();
            }
            Err("analysis program exceeded its execution time".to_string())
        }
    }
}

struct IsolateExecutionGuard {
    isolate_handle: v8::IsolateHandle,
    armed: bool,
}

impl IsolateExecutionGuard {
    fn new(isolate_handle: v8::IsolateHandle) -> Self {
        Self {
            isolate_handle,
            armed: true,
        }
    }

    fn terminate(&self) {
        self.isolate_handle.terminate_execution();
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for IsolateExecutionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.terminate();
        }
    }
}

fn run_analysis_isolate(
    inputs: Arc<ResolvedWorkflowInputs>,
    program: &str,
    isolate_tx: tokio::sync::oneshot::Sender<v8::IsolateHandle>,
    execution_guard_ready_rx: tokio::sync::oneshot::Receiver<()>,
    program_ready_tx: tokio::sync::oneshot::Sender<()>,
    _permit: OwnedSemaphorePermit,
) -> Result<AnalysisOutput, String> {
    initialize_v8(V8JitMode::Enabled)?;
    let create_params = v8::CreateParams::default().heap_limits(0, ANALYSIS_HEAP_LIMIT_BYTES);
    let mut isolate = v8::Isolate::new(create_params);
    let isolate_handle = isolate.thread_safe_handle();
    let mut heap_state = Box::new(HeapLimitState {
        exceeded: AtomicBool::new(false),
        emergency_granted: AtomicBool::new(false),
        isolate_handle: isolate_handle.clone(),
    });
    let heap_state_ptr = (&mut *heap_state as *mut HeapLimitState).cast::<c_void>();
    isolate.add_near_heap_limit_callback(near_heap_limit, heap_state_ptr);
    if isolate_tx.send(isolate_handle.clone()).is_err() {
        isolate.remove_near_heap_limit_callback(near_heap_limit, ANALYSIS_HEAP_LIMIT_BYTES);
        drop(heap_state);
        return Err("analysis request ended before isolate initialization".to_string());
    }
    if execution_guard_ready_rx.blocking_recv().is_err() {
        isolate.remove_near_heap_limit_callback(near_heap_limit, ANALYSIS_HEAP_LIMIT_BYTES);
        drop(heap_state);
        return Err("analysis request ended before execution guard setup".to_string());
    }

    let result = run_analysis_context(
        &mut isolate,
        &inputs,
        program,
        program_ready_tx,
        &heap_state,
    );
    let heap_exceeded = heap_state.exceeded.load(Ordering::Acquire);
    let execution_terminated = isolate_handle.is_execution_terminating();
    if execution_terminated {
        isolate_handle.cancel_terminate_execution();
    }
    isolate.remove_near_heap_limit_callback(near_heap_limit, ANALYSIS_HEAP_LIMIT_BYTES);
    drop(heap_state);

    if heap_exceeded {
        Err("analysis program exceeded the available V8 heap".to_string())
    } else if execution_terminated {
        Err("workflow input analysis was terminated".to_string())
    } else {
        result
    }
}

fn run_analysis_context(
    isolate: &mut v8::OwnedIsolate,
    inputs: &ResolvedWorkflowInputs,
    program: &str,
    program_ready_tx: tokio::sync::oneshot::Sender<()>,
    heap_state: &HeapLimitState,
) -> Result<AnalysisOutput, String> {
    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    context.set_allow_generation_from_strings(false);
    let scope = &mut v8::ContextScope::new(scope, context);
    scope.set_slot(AnalysisIsolateState::default());
    install_analysis_globals(scope, inputs, heap_state)?;
    let _ = program_ready_tx.send(());

    let wrapped = format!("(function () {{\n\"use strict\";\n{program}\n}})()");
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let source = v8::String::new(&tc, &wrapped)
        .ok_or_else(|| "failed to allocate analysis source".to_string())?;
    let script = v8::Script::compile(&tc, source, None).ok_or_else(|| {
        tc.exception()
            .map(|exception| bounded_exception_text(&mut tc, exception, MAX_MODEL_ERROR_BYTES))
            .unwrap_or_else(|| "analysis program did not compile".to_string())
    })?;
    let result = script.run(&tc).ok_or_else(|| {
        tc.exception()
            .map(|exception| bounded_exception_text(&mut tc, exception, MAX_MODEL_ERROR_BYTES))
            .unwrap_or_else(|| "analysis program failed".to_string())
    })?;
    if result.is_promise() {
        return Err("return the analysis result synchronously".to_string());
    }
    let result = serialize_analysis_result(&mut tc, result)?;
    let state = tc
        .get_slot::<AnalysisIsolateState>()
        .ok_or_else(|| "analysis log state is unavailable".to_string())?;
    Ok(AnalysisOutput {
        result,
        logs: state.logs.clone(),
        logs_truncated: state.logs_truncated,
    })
}

#[derive(Default)]
struct AnalysisIsolateState {
    logs: Vec<String>,
    log_bytes: usize,
    logs_truncated: bool,
}

struct HeapLimitState {
    exceeded: AtomicBool,
    emergency_granted: AtomicBool,
    isolate_handle: v8::IsolateHandle,
}

unsafe extern "C" fn near_heap_limit(
    data: *mut c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    // SAFETY: `data` points to the boxed state kept alive until the callback is removed.
    let state = unsafe { &*(data.cast::<HeapLimitState>()) };
    state.exceeded.store(true, Ordering::Release);
    state.isolate_handle.terminate_execution();
    if state.emergency_granted.swap(true, Ordering::AcqRel) {
        current_heap_limit
    } else {
        current_heap_limit.saturating_add(ANALYSIS_HEAP_EMERGENCY_BYTES)
    }
}

fn install_analysis_globals(
    scope: &mut v8::PinScope<'_, '_>,
    inputs: &ResolvedWorkflowInputs,
    heap_state: &HeapLimitState,
) -> Result<(), String> {
    let global = scope.get_current_context().global(scope);
    let inputs_key = v8_string(scope, "inputs")?;
    let inputs_value = v8::Object::new(scope);
    let mut injection = InjectionState::new(heap_state);
    let mut injected_artifacts = HashMap::new();
    let mut active_artifacts = HashSet::new();
    for (alias, reference) in inputs.references() {
        injection.poll()?;
        let key = v8_string(scope, alias)?;
        let injected = materialize_input_artifact(
            scope,
            inputs,
            reference,
            &mut injected_artifacts,
            &mut active_artifacts,
            &mut injection,
            1,
        )?;
        if inputs_value.create_data_property(scope, key.into(), injected) != Some(true) {
            return Err("failed to inject workflow input alias".to_string());
        }
    }
    freeze_object(scope, inputs_value)?;
    define_read_only(scope, global, inputs_key.into(), inputs_value.into())?;

    let utf8_slice = v8::FunctionTemplate::new(scope, utf8_slice_callback)
        .get_function(scope)
        .ok_or_else(|| "failed to create utf8Slice helper".to_string())?;
    let helpers = v8::Object::new(scope);
    let utf8_slice_key = v8_string(scope, "utf8Slice")?;
    define_read_only(scope, helpers, utf8_slice_key.into(), utf8_slice.into())?;
    freeze_object(scope, helpers)?;
    let helpers_key = v8_string(scope, "helpers")?;
    define_read_only(scope, global, helpers_key.into(), helpers.into())?;

    let console = v8::Object::new(scope);
    for name in ["log", "info", "warn", "error"] {
        let callback = v8::FunctionTemplate::new(scope, console_log_callback)
            .get_function(scope)
            .ok_or_else(|| "failed to create analysis console callback".to_string())?;
        let key = v8_string(scope, name)?;
        define_read_only(scope, console, key.into(), callback.into())?;
    }
    freeze_object(scope, console)?;
    let console_key = v8_string(scope, "console")?;
    define_read_only(scope, global, console_key.into(), console.into())?;

    for name in [
        "ArrayBuffer",
        "BigInt64Array",
        "BigUint64Array",
        "DataView",
        "Date",
        "Float16Array",
        "Float32Array",
        "Float64Array",
        "Int8Array",
        "Int16Array",
        "Int32Array",
        "Temporal",
        "Intl",
        "performance",
        "crypto",
        "Atomics",
        "SharedArrayBuffer",
        "Uint8Array",
        "Uint8ClampedArray",
        "Uint16Array",
        "Uint32Array",
        "WebAssembly",
    ] {
        remove_global(scope, global, name)?;
    }
    disable_random(scope, global)?;
    freeze_builtin_prototype(scope, global, "Object")?;
    freeze_builtin_prototype(scope, global, "Array")?;
    Ok(())
}

fn materialize_input_artifact<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    inputs: &ResolvedWorkflowInputs,
    reference: &WorkflowInputArtifactRef,
    cache: &mut HashMap<WorkflowInputArtifactRef, v8::Local<'s, v8::Value>>,
    active: &mut HashSet<WorkflowInputArtifactRef>,
    injection: &mut InjectionState<'_>,
    depth: usize,
) -> Result<v8::Local<'s, v8::Value>, String> {
    injection.poll()?;
    if depth > INPUT_RECURSION_GUARD {
        return Err("use a flatter structured value for workflow inputs".to_string());
    }
    if let Some(value) = cache.get(reference) {
        return Ok(*value);
    }
    if !active.insert(reference.clone()) {
        return Err("workflow input artifact references must be acyclic".to_string());
    }
    let value = match reference.kind {
        WorkflowInputArtifactKind::Value => {
            let value = inputs.value(reference).ok_or_else(|| {
                format!(
                    "workflow input artifact {} is unavailable",
                    reference.sha256
                )
            })?;
            json_to_frozen_v8(scope, value, injection, depth)?
        }
        WorkflowInputArtifactKind::Descriptor => {
            let descriptor = inputs.descriptor(reference).ok_or_else(|| {
                format!(
                    "workflow input descriptor {} is unavailable",
                    reference.sha256
                )
            })?;
            descriptor_to_frozen_v8(scope, inputs, descriptor, cache, active, injection, depth)?
        }
    };
    active.remove(reference);
    cache.insert(reference.clone(), value);
    Ok(value)
}

fn descriptor_to_frozen_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    inputs: &ResolvedWorkflowInputs,
    descriptor: &WorkflowInputDescriptor,
    cache: &mut HashMap<WorkflowInputArtifactRef, v8::Local<'s, v8::Value>>,
    active: &mut HashSet<WorkflowInputArtifactRef>,
    injection: &mut InjectionState<'_>,
    depth: usize,
) -> Result<v8::Local<'s, v8::Value>, String> {
    let artifacts = descriptor
        .artifacts
        .iter()
        .map(|artifact| (artifact.path.as_slice(), &artifact.reference))
        .collect::<HashMap<_, _>>();
    let negative_zeros = descriptor
        .negative_zeros
        .iter()
        .map(Vec::as_slice)
        .collect::<HashSet<_>>();
    descriptor_value_to_frozen_v8(
        scope,
        inputs,
        &descriptor.value,
        &artifacts,
        &negative_zeros,
        cache,
        active,
        injection,
        &mut Vec::new(),
        depth,
    )
}

#[allow(clippy::too_many_arguments)]
fn descriptor_value_to_frozen_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    inputs: &ResolvedWorkflowInputs,
    value: &JsonValue,
    artifacts: &HashMap<&[WorkflowInputPathSegment], &WorkflowInputArtifactRef>,
    negative_zeros: &HashSet<&[WorkflowInputPathSegment]>,
    cache: &mut HashMap<WorkflowInputArtifactRef, v8::Local<'s, v8::Value>>,
    active: &mut HashSet<WorkflowInputArtifactRef>,
    injection: &mut InjectionState<'_>,
    path: &mut Vec<WorkflowInputPathSegment>,
    depth: usize,
) -> Result<v8::Local<'s, v8::Value>, String> {
    injection.poll()?;
    if depth > INPUT_RECURSION_GUARD {
        return Err("use a flatter structured value for workflow inputs".to_string());
    }
    if let Some(reference) = artifacts.get(path.as_slice()) {
        return materialize_input_artifact(
            scope, inputs, reference, cache, active, injection, depth,
        );
    }
    if negative_zeros.contains(path.as_slice()) {
        return Ok(v8::Number::new(scope, -0.0).into());
    }
    match value {
        JsonValue::Array(values) => {
            let array = v8::Array::new(scope, values.len() as i32);
            for (index, value) in values.iter().enumerate() {
                path.push(WorkflowInputPathSegment::Index(index));
                let value = descriptor_value_to_frozen_v8(
                    scope,
                    inputs,
                    value,
                    artifacts,
                    negative_zeros,
                    cache,
                    active,
                    injection,
                    path,
                    depth + 1,
                )?;
                path.pop();
                if array.set_index(scope, index as u32, value) != Some(true) {
                    return Err("failed to inject workflow input array".to_string());
                }
            }
            freeze_object(scope, array.into())?;
            Ok(array.into())
        }
        JsonValue::Object(values) => {
            let object = v8::Object::new(scope);
            for (key, value) in values {
                path.push(WorkflowInputPathSegment::Key(key.clone()));
                let injected = descriptor_value_to_frozen_v8(
                    scope,
                    inputs,
                    value,
                    artifacts,
                    negative_zeros,
                    cache,
                    active,
                    injection,
                    path,
                    depth + 1,
                )?;
                path.pop();
                let key = v8_string(scope, key)?;
                if object.create_data_property(scope, key.into(), injected) != Some(true) {
                    return Err("failed to inject workflow input object".to_string());
                }
            }
            freeze_object(scope, object)?;
            Ok(object.into())
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {
            json_to_frozen_v8(scope, value, injection, depth)
        }
    }
}

fn json_to_frozen_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &JsonValue,
    injection: &mut InjectionState<'_>,
    depth: usize,
) -> Result<v8::Local<'s, v8::Value>, String> {
    injection.poll()?;
    if depth > INPUT_RECURSION_GUARD {
        return Err("use a flatter structured value for workflow inputs".to_string());
    }
    match value {
        JsonValue::Null => Ok(v8::null(scope).into()),
        JsonValue::Bool(value) => Ok(v8::Boolean::new(scope, *value).into()),
        JsonValue::Number(value) => {
            let number = value
                .as_f64()
                .ok_or_else(|| "represent this workflow input number as a string".to_string())?;
            Ok(v8::Number::new(scope, number).into())
        }
        JsonValue::String(value) => Ok(v8_string(scope, value)?.into()),
        JsonValue::Array(values) => {
            let array = v8::Array::new(scope, values.len() as i32);
            for (index, value) in values.iter().enumerate() {
                let value = json_to_frozen_v8(scope, value, injection, depth + 1)?;
                if array.set_index(scope, index as u32, value) != Some(true) {
                    return Err("failed to inject workflow input array".to_string());
                }
            }
            freeze_object(scope, array.into())?;
            Ok(array.into())
        }
        JsonValue::Object(values) => {
            let object = v8::Object::new(scope);
            for (key, value) in values {
                let key = v8_string(scope, key)?;
                let value = json_to_frozen_v8(scope, value, injection, depth + 1)?;
                if object.create_data_property(scope, key.into(), value) != Some(true) {
                    return Err("failed to inject workflow input object".to_string());
                }
            }
            freeze_object(scope, object)?;
            Ok(object.into())
        }
    }
}

struct InjectionState<'a> {
    heap_state: &'a HeapLimitState,
}

impl<'a> InjectionState<'a> {
    fn new(heap_state: &'a HeapLimitState) -> Self {
        Self { heap_state }
    }

    fn poll(&mut self) -> Result<(), String> {
        if self.heap_state.exceeded.load(Ordering::Acquire)
            || self.heap_state.isolate_handle.is_execution_terminating()
        {
            return Err("workflow input injection was terminated".to_string());
        }
        Ok(())
    }
}

fn serialize_analysis_result(
    scope: &mut v8::PinScope<'_, '_>,
    result: v8::Local<'_, v8::Value>,
) -> Result<JsonValue, String> {
    let serialized = v8::json::stringify(scope, result)
        .ok_or_else(|| "return a JSON-compatible analysis result".to_string())?;
    let mut parsed: JsonValue = serde_json::from_str(&serialized.to_rust_string_lossy(scope))
        .map_err(|error| format!("analysis result is not valid JSON: {error}"))?;
    restore_negative_zero(scope, result, &mut parsed)?;
    Ok(parsed)
}

fn restore_negative_zero(
    scope: &mut v8::PinScope<'_, '_>,
    source: v8::Local<'_, v8::Value>,
    result: &mut JsonValue,
) -> Result<(), String> {
    match result {
        JsonValue::Number(number) if source.is_number() => {
            let Some(value) = source.number_value(scope) else {
                return Err("return a JSON-compatible analysis result".to_string());
            };
            if value == 0.0 && value.is_sign_negative() {
                *number = serde_json::Number::from_f64(-0.0)
                    .ok_or_else(|| "failed to preserve analysis number".to_string())?;
            }
        }
        JsonValue::Array(values) if source.is_array() => {
            let array = v8::Local::<v8::Array>::try_from(source)
                .map_err(|_| "return a JSON-compatible analysis result".to_string())?;
            for (index, value) in values.iter_mut().enumerate() {
                let source = array
                    .get_index(scope, index as u32)
                    .ok_or_else(|| "failed to read analysis result array".to_string())?;
                restore_negative_zero(scope, source, value)?;
            }
        }
        JsonValue::Object(values) if source.is_object() => {
            let object = v8::Local::<v8::Object>::try_from(source)
                .map_err(|_| "return a JSON-compatible analysis result".to_string())?;
            for (key, value) in values {
                let key = v8_string(scope, key)?;
                let source = object
                    .get(scope, key.into())
                    .ok_or_else(|| "failed to read analysis result object".to_string())?;
                restore_negative_zero(scope, source, value)?;
            }
        }
        JsonValue::Null
        | JsonValue::Bool(_)
        | JsonValue::Number(_)
        | JsonValue::String(_)
        | JsonValue::Array(_)
        | JsonValue::Object(_) => {}
    }
    Ok(())
}

fn console_log_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue<v8::Value>,
) {
    let argument_count = args.length();
    let mut value_truncated = argument_count > MAX_ANALYSIS_LOG_ARGUMENTS;
    let mut parts = Vec::new();
    for index in 0..argument_count.min(MAX_ANALYSIS_LOG_ARGUMENTS) {
        let (part, truncated) =
            bounded_v8_text_with_truncation(scope, args.get(index), MAX_ANALYSIS_LOG_BYTES);
        parts.push(part);
        value_truncated |= truncated;
    }
    let line = parts.join(" ");
    let Some(state) = scope.get_slot_mut::<AnalysisIsolateState>() else {
        return;
    };
    state.logs_truncated |= value_truncated;
    let separator_bytes = usize::from(!state.logs.is_empty());
    let remaining = MAX_ANALYSIS_LOG_BYTES.saturating_sub(state.log_bytes + separator_bytes);
    if remaining == 0 {
        state.logs_truncated = true;
        return;
    }
    let bounded = bounded_text(&line, remaining);
    if bounded.len() < line.len() {
        state.logs_truncated = true;
    }
    state.log_bytes += separator_bytes + bounded.len();
    state.logs.push(bounded);
}

fn utf8_slice_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let value = args.get(0);
    if !value.is_string() {
        throw_type_error(scope, "helpers.utf8Slice expects a string");
        return;
    }
    let start = args.get(1).integer_value(scope).unwrap_or(0).max(0) as usize;
    let maximum = args
        .get(2)
        .integer_value(scope)
        .unwrap_or(MAX_HELPER_SLICE_BYTES as i64)
        .clamp(0, MAX_HELPER_SLICE_BYTES as i64) as usize;
    let value = value.to_rust_string_lossy(scope);
    let start = ceil_char_boundary(&value, start.min(value.len()));
    let end = floor_char_boundary(&value, start.saturating_add(maximum).min(value.len()));
    let Some(result) = v8::String::new(scope, &value[start..end]) else {
        throw_type_error(scope, "helpers.utf8Slice could not allocate its result");
        return;
    };
    retval.set(result.into());
}

fn bounded_v8_text_with_truncation(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    maximum: usize,
) -> (String, bool) {
    if value.is_array() {
        let length = v8::Local::<v8::Array>::try_from(value)
            .map(|array| array.length())
            .unwrap_or_default();
        return (format!("[Array({length})]"), false);
    }
    if value.is_object() {
        return ("[Object]".to_string(), false);
    }
    let value = if value.is_string() {
        v8::Local::<v8::String>::try_from(value).ok()
    } else {
        value.to_string(scope)
    };
    let Some(value) = value else {
        return ("[unserializable]".to_string(), false);
    };
    if value.utf8_length(scope) > maximum {
        return ("[log value truncated]".to_string(), true);
    }
    let value = value.to_rust_string_lossy(scope);
    let bounded = bounded_text(&value, maximum);
    let truncated = bounded.len() < value.len();
    (bounded, truncated)
}

fn bounded_exception_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    maximum: usize,
) -> String {
    if value.is_object()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(value)
        && let Ok(stack_key) = v8_string(scope, "stack")
        && let Some(stack) = object.get(scope, stack_key.into())
        && stack.is_string()
        && let Ok(stack) = v8::Local::<v8::String>::try_from(stack)
    {
        return bounded_v8_string(scope, stack, maximum);
    }
    let Some(value) = value.to_string(scope) else {
        return "analysis program failed".to_string();
    };
    bounded_v8_string(scope, value, maximum)
}

fn bounded_v8_string(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::String>,
    maximum: usize,
) -> String {
    let mut bytes = vec![0; value.utf8_length(scope).min(maximum)];
    let written = value.write_utf8_v2(scope, &mut bytes, v8::WriteFlags::kReplaceInvalidUtf8, None);
    bytes.truncate(written);
    bounded_text(&String::from_utf8_lossy(&bytes), maximum)
}

fn analyze_workflow_inputs_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: ANALYZE_WORKFLOW_INPUTS_TOOL_NAME.to_string(),
        description: "Analyze complete deep-frozen workflow inputs with a synchronous pure JavaScript program. Discover available input names with `Object.keys(globalThis.inputs)`, then access values through `globalThis.inputs`. Return a JSON-compatible result with `return`, use `console.log` for diagnostics, and use `helpers.utf8Slice(value, startByte, maxBytes)` for UTF-8 byte slicing."
            .to_string(),
        strict: true,
        parameters: parse_tool_input_schema(&json!({
            "type": "object",
            "properties": {
                "program": {
                    "type": "string",
                    "description": "Synchronous JavaScript function body ending in a return statement.",
                }
            },
            "required": ["program"],
            "additionalProperties": false
        }))
        .expect("AnalyzeWorkflowInputs schema must be valid"),
        output_schema: None,
        defer_loading: None,
    })
}

fn define_read_only<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<'s, v8::Object>,
    key: v8::Local<'s, v8::Name>,
    value: v8::Local<'s, v8::Value>,
) -> Result<(), String> {
    let attributes = v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_DELETE;
    if object.define_own_property(scope, key, value, attributes) == Some(true) {
        Ok(())
    } else {
        Err("failed to define analysis global".to_string())
    }
}

fn freeze_object(
    scope: &mut v8::PinScope<'_, '_>,
    object: v8::Local<'_, v8::Object>,
) -> Result<(), String> {
    if object.set_integrity_level(scope, v8::IntegrityLevel::Frozen) == Some(true) {
        Ok(())
    } else {
        Err("failed to freeze workflow input value".to_string())
    }
}

fn remove_global(
    scope: &mut v8::PinScope<'_, '_>,
    global: v8::Local<'_, v8::Object>,
    name: &str,
) -> Result<(), String> {
    let key = v8_string(scope, name)?;
    if global.delete(scope, key.into()) == Some(true) {
        Ok(())
    } else {
        Err(format!("failed to remove analysis global `{name}`"))
    }
}

fn disable_random(
    scope: &mut v8::PinScope<'_, '_>,
    global: v8::Local<'_, v8::Object>,
) -> Result<(), String> {
    let math_key = v8_string(scope, "Math")?;
    let math = global
        .get(scope, math_key.into())
        .and_then(|value| v8::Local::<v8::Object>::try_from(value).ok())
        .ok_or_else(|| "analysis Math global is unavailable".to_string())?;
    let random_key = v8_string(scope, "random")?;
    define_read_only(scope, math, random_key.into(), v8::undefined(scope).into())?;
    freeze_object(scope, math)
}

fn freeze_builtin_prototype(
    scope: &mut v8::PinScope<'_, '_>,
    global: v8::Local<'_, v8::Object>,
    name: &str,
) -> Result<(), String> {
    let constructor_key = v8_string(scope, name)?;
    let constructor = global
        .get(scope, constructor_key.into())
        .and_then(|value| v8::Local::<v8::Object>::try_from(value).ok())
        .ok_or_else(|| format!("analysis {name} constructor is unavailable"))?;
    let prototype_key = v8_string(scope, "prototype")?;
    let prototype = constructor
        .get(scope, prototype_key.into())
        .and_then(|value| v8::Local::<v8::Object>::try_from(value).ok())
        .ok_or_else(|| format!("analysis {name} prototype is unavailable"))?;
    freeze_object(scope, prototype)
}

fn v8_string<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &str,
) -> Result<v8::Local<'s, v8::String>, String> {
    v8::String::new(scope, value).ok_or_else(|| "failed to allocate V8 string".to_string())
}

fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    if let Some(message) = v8::String::new(scope, message) {
        scope.throw_exception(message.into());
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn bounded_text(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_string();
    }
    let marker = "...";
    if maximum <= marker.len() {
        return value[..floor_char_boundary(value, maximum)].to_string();
    }
    let end = floor_char_boundary(value, maximum - marker.len());
    format!("{}{marker}", &value[..end])
}

#[cfg(test)]
#[path = "analyze_inputs_tests.rs"]
mod tests;
