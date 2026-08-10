const __wfMaxItems = 4096;
const __wfMaxTimers = 64;
const __wfMaxProgressTextBytes = 256;
const __wfMaxParallelErrorBytes = 512;
const __wfInputRecursionGuard = 256;
const __wfResultSanitizeLimits = Object.freeze({
  maxDepth: 64,
  errorName: "WorkflowResultLimitError",
  label: "workflow result",
});
const __wfSchemaSanitizeLimits = Object.freeze({
  maxDepth: 64,
  maxNodes: 4 * 1024,
  errorName: "WorkflowSchemaLimitError",
  label: "workflow agent schema",
});
const __wfValueSanitizeLimits = Object.freeze({
  maxDepth: 64,
  errorName: "WorkflowValueLimitError",
  label: "workflow value",
});
let __wfNextAgentIndex = 0;
let __wfNextInvocationGroup = 0;
let __wfNextChildInvocation = 0;
let __wfRunTokens = 0;
let __wfPhaseIndex = __wfInitialPhaseIndex ?? undefined;
let __wfPhaseTitle = __wfInitialPhaseTitle ?? undefined;
const __wfHostNotify = globalThis.notify;
const __wfHostAgent = globalThis.tools.workflow_agent;
const __wfHostChild = globalThis.tools.workflow_child;
const __wfHostDeclaredInput = globalThis.tools.workflow_declared_input;
const __wfHostInputArtifact = globalThis.tools.workflow_input_artifact;
const __wfHostResult = globalThis.tools[__wfResultToolName];
const __wfHostSetTimeout = globalThis.setTimeout;
const __wfHostClearTimeout = globalThis.clearTimeout;
const __wfHostContinuationScope = globalThis.__codexContinuationScope;
const __wfHostSetContinuationScope = globalThis.__codexSetContinuationScope;
const __wfActiveTimers = new Set();
const __wfObjectArtifacts = new WeakMap();

delete globalThis.__codexContinuationScope;
delete globalThis.__codexSetContinuationScope;

function __wfSetTimeout(callback, delay = 0) {
  if (typeof callback !== "function") throw new TypeError("setTimeout expects a function callback");
  if (__wfActiveTimers.size >= __wfMaxTimers) {
    throw new RangeError("clear or await existing workflow timers before creating more");
  }
  let timeoutId;
  timeoutId = __wfHostSetTimeout(() => {
    __wfActiveTimers.delete(timeoutId);
    callback();
  }, delay);
  __wfActiveTimers.add(timeoutId);
  return timeoutId;
}

function __wfClearTimeout(timeoutId) {
  __wfActiveTimers.delete(timeoutId);
  __wfHostClearTimeout(timeoutId);
}

Object.defineProperties(globalThis, {
  setTimeout: { value: Object.freeze(__wfSetTimeout), writable: false, configurable: false },
  clearTimeout: { value: Object.freeze(__wfClearTimeout), writable: false, configurable: false },
});

for (const __wfName of __wfUnavailableGlobalNames) {
  try { delete globalThis[__wfName]; } catch (_) { globalThis[__wfName] = undefined; }
}

const __wfPhaseTitles = JSON.parse(__wfPhaseTitlesJson);
const __wfPhaseIndices = new Map(__wfPhaseTitles.map((title, index) => [title, index]));
let __wfNextPhaseIndex = __wfPhaseTitles.length;

function __wfErrorText(error) {
  if (error instanceof Error) return error.message;
  return String(error);
}

function __wfNotify(value) {
  __wfHostNotify(JSON.stringify(value));
}

function __wfAllocateInvocationGroup(kind) {
  const scope = __wfHostContinuationScope();
  if (scope !== undefined) {
    const index = scope.nextGroup++;
    return `${scope.path}/${kind}:${index}`;
  }
  return `${kind}:${__wfNextInvocationGroup++}`;
}

function __wfReserveAgentScope(path) {
  return {
    path,
    index: __wfNextAgentIndex++,
    nextAgent: 0,
    nextChild: 0,
    nextGroup: 0,
  };
}

function __wfWithInvocationScope(scope, callback) {
  const previous = __wfHostContinuationScope();
  __wfHostSetContinuationScope(scope);
  try {
    return callback();
  } finally {
    __wfHostSetContinuationScope(previous);
  }
}

function __wfAgentInvocation() {
  const scope = __wfHostContinuationScope();
  if (scope === undefined) {
    const index = __wfNextAgentIndex++;
    return { index, invocationId: `agent:${index}` };
  }
  const ordinal = scope.nextAgent++;
  const index = ordinal === 0 ? scope.index : __wfNextAgentIndex++;
  return { index, invocationId: `${scope.path}/agent:${ordinal}` };
}

function __wfChildInvocation() {
  const scope = __wfHostContinuationScope();
  if (scope === undefined) return `workflow:${__wfNextChildInvocation++}`;
  return `${scope.path}/workflow:${scope.nextChild++}`;
}

function __wfResolvePhase(title, declareIfNew = false) {
  if (title === undefined) return undefined;
  let index = __wfPhaseIndices.get(title);
  if (index !== undefined) return index;
  index = __wfNextPhaseIndex++;
  __wfPhaseIndices.set(title, index);
  if (declareIfNew) {
    __wfNotify({ type: "workflow_phase", index, title, kind: "declared" });
  }
  return index;
}

function __wfThrowSanitizeLimit(limits, kind) {
  if (limits === __wfResultSanitizeLimits && kind === "depth") {
    throw "WorkflowResultLimitError: return a shallower workflow result";
  }
  throw `${limits.errorName}: use a focused ${limits.label}; split larger material across additional calls or artifacts`;
}

function __wfChargeSanitizeSize(state, limits, kind, amount) {
  const maximum = kind === "bytes" ? limits.maxBytes : limits.maxNodes;
  if (maximum === undefined) return;
  if (amount > maximum - state[kind]) {
    __wfThrowSanitizeLimit(limits, kind);
  }
  state[kind] += amount;
}

function __wfChargeJsonString(state, limits, value) {
  if (limits.maxBytes === undefined) return;
  __wfChargeSanitizeSize(state, limits, "bytes", 2);
  for (let index = 0; index < value.length; index++) {
    const codeUnit = value.charCodeAt(index);
    let bytes;
    if (codeUnit === 0x22 || codeUnit === 0x5c || codeUnit === 0x08 ||
        codeUnit === 0x09 || codeUnit === 0x0a || codeUnit === 0x0c || codeUnit === 0x0d) {
      bytes = 2;
    } else if (codeUnit <= 0x1f) {
      bytes = 6;
    } else if (codeUnit <= 0x7f) {
      bytes = 1;
    } else if (codeUnit <= 0x7ff) {
      bytes = 2;
    } else if (codeUnit >= 0xd800 && codeUnit <= 0xdbff) {
      const next = value.charCodeAt(index + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        bytes = 4;
        index += 1;
      } else {
        bytes = 6;
      }
    } else if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {
      bytes = 6;
    } else {
      bytes = 3;
    }
    __wfChargeSanitizeSize(state, limits, "bytes", bytes);
  }
}

function __wfSanitizeValue(value, depth, state, limits) {
  if (depth > limits.maxDepth) __wfThrowSanitizeLimit(limits, "depth");
  if (value === undefined) value = null;
  if (value === null) {
    __wfChargeSanitizeSize(state, limits, "nodes", 1);
    __wfChargeSanitizeSize(state, limits, "bytes", 4);
    return { value, height: 1 };
  }
  if (typeof value === "string") {
    __wfChargeSanitizeSize(state, limits, "nodes", 1);
    __wfChargeJsonString(state, limits, value);
    return { value, height: 1 };
  }
  if (typeof value === "boolean") {
    __wfChargeSanitizeSize(state, limits, "nodes", 1);
    __wfChargeSanitizeSize(state, limits, "bytes", value ? 4 : 5);
    return { value, height: 1 };
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError("workflow values must contain finite numbers");
    __wfChargeSanitizeSize(state, limits, "nodes", 1);
    __wfChargeSanitizeSize(state, limits, "bytes", String(value).length);
    return { value, height: 1 };
  }
  if (typeof value !== "object") throw new TypeError("workflow values must be JSON-compatible");
  if (state.active.has(value)) throw new TypeError("workflow values must be acyclic JSON-compatible values");
  const cached = state.completed.get(value);
  if (cached !== undefined) {
    if (depth + cached.height - 1 > limits.maxDepth) {
      __wfThrowSanitizeLimit(limits, "depth");
    }
    __wfChargeSanitizeSize(state, limits, "nodes", cached.nodes);
    __wfChargeSanitizeSize(state, limits, "bytes", cached.bytes);
    return { value: cached.value, height: cached.height };
  }

  const initialNodes = state.nodes;
  const initialBytes = state.bytes;
  __wfChargeSanitizeSize(state, limits, "nodes", 1);
  state.active.add(value);
  let result;
  let height = 1;
  if (Array.isArray(value)) {
    if (limits === __wfSchemaSanitizeLimits && value.length > __wfMaxItems) {
      throw new RangeError("pass a focused workflow array; split larger material across additional calls or artifacts");
    }
    __wfChargeSanitizeSize(
      state,
      limits,
      "bytes",
      2 + Math.max(0, value.length - 1),
    );
    result = [];
    for (let index = 0; index < value.length; index++) {
      const child = __wfSanitizeValue(value[index], depth + 1, state, limits);
      result.push(child.value);
      height = Math.max(height, child.height + 1);
    }
  } else {
    result = {};
    __wfChargeSanitizeSize(state, limits, "bytes", 2);
    let entryCount = 0;
    for (const key in value) {
      if (!Object.prototype.hasOwnProperty.call(value, key)) continue;
      entryCount += 1;
      if (limits === __wfSchemaSanitizeLimits && entryCount > __wfMaxItems) {
        throw new RangeError("pass a focused workflow object; split larger material across additional calls or artifacts");
      }
      if (entryCount > 1) __wfChargeSanitizeSize(state, limits, "bytes", 1);
      __wfChargeJsonString(state, limits, key);
      __wfChargeSanitizeSize(state, limits, "bytes", 1);
      const child = __wfSanitizeValue(value[key], depth + 1, state, limits);
      Object.defineProperty(result, key, {
        value: child.value,
        enumerable: true,
        writable: true,
        configurable: true,
      });
      height = Math.max(height, child.height + 1);
    }
  }
  state.active.delete(value);
  state.completed.set(value, {
    value: result,
    height,
    nodes: state.nodes - initialNodes,
    bytes: state.bytes - initialBytes,
  });
  return { value: result, height };
}

function __wfSanitize(value, limits = __wfResultSanitizeLimits) {
  return __wfSanitizeValue(value, 1, {
    bytes: 0,
    nodes: 0,
    active: new WeakSet(),
    completed: new WeakMap(),
  }, limits).value;
}

function __wfDeepFreeze(value, seen = new WeakSet()) {
  if (value === null || typeof value !== "object" || seen.has(value)) return value;
  seen.add(value);
  for (const child of Object.values(value)) __wfDeepFreeze(child, seen);
  return Object.freeze(value);
}

function __wfNamedError(name, message) {
  const error = new Error(message);
  Object.defineProperty(error, "name", { value: name });
  return error;
}

function __wfUtf8ByteLength(value) {
  let bytes = 0;
  for (let index = 0; index < value.length; index++) {
    const codeUnit = value.charCodeAt(index);
    if (codeUnit <= 0x7f) {
      bytes += 1;
    } else if (codeUnit <= 0x7ff) {
      bytes += 2;
    } else if (
      codeUnit >= 0xd800 &&
      codeUnit <= 0xdbff &&
      index + 1 < value.length &&
      value.charCodeAt(index + 1) >= 0xdc00 &&
      value.charCodeAt(index + 1) <= 0xdfff
    ) {
      bytes += 4;
      index += 1;
    } else {
      bytes += 3;
    }
  }
  return bytes;
}

function __wfUtf8Prefix(value, maxBytes) {
  if (maxBytes <= 0) return "";
  let bytes = 0;
  let end = 0;
  for (let index = 0; index < value.length;) {
    const codeUnit = value.charCodeAt(index);
    let width;
    let codeUnits = 1;
    if (codeUnit <= 0x7f) {
      width = 1;
    } else if (codeUnit <= 0x7ff) {
      width = 2;
    } else if (
      codeUnit >= 0xd800 &&
      codeUnit <= 0xdbff &&
      index + 1 < value.length &&
      value.charCodeAt(index + 1) >= 0xdc00 &&
      value.charCodeAt(index + 1) <= 0xdfff
    ) {
      width = 4;
      codeUnits = 2;
    } else {
      width = 3;
    }
    if (bytes + width > maxBytes) break;
    bytes += width;
    index += codeUnits;
    end = index;
  }
  return value.slice(0, end);
}

function __wfTruncateUtf8(value, maxBytes, marker = "") {
  if (__wfUtf8ByteLength(value) <= maxBytes) return value;
  const markerBytes = __wfUtf8ByteLength(marker);
  if (markerBytes >= maxBytes) return __wfUtf8Prefix(value, maxBytes);
  return __wfUtf8Prefix(value, maxBytes - markerBytes) + marker;
}

function __wfPromptOptions(options) {
  if (options === null || typeof options !== "object" || Array.isArray(options)) {
    throw new TypeError("workflow agent prompt options must be an object");
  }
  const effective = { ...options };
  if (effective.schema !== undefined) {
    effective.schema = __wfSanitize(effective.schema, __wfSchemaSanitizeLimits);
  }
  return effective;
}

function __wfAgentOptions(options) {
  const effective = __wfPromptOptions(options);
  if (effective.inputs === undefined) return effective;
  if (effective.inputs === null || typeof effective.inputs !== "object" ||
      Array.isArray(effective.inputs)) {
    throw new TypeError("workflow agent inputs must be an object of named values");
  }
  const aliases = Object.keys(effective.inputs);
  if (aliases.length === 0) {
    throw new TypeError(
      "provide at least one named structured value through agent(..., { inputs })",
    );
  }
  for (const alias of aliases) {
    if (alias.trim().length === 0) {
      throw new TypeError("provide a short name for every workflow agent input");
    }
  }
  return effective;
}

function __wfRememberArtifact(value, artifact) {
  if (artifact === null || artifact === undefined) return;
  if (value !== null && typeof value === "object") {
    __wfObjectArtifacts.set(value, artifact);
  }
}

function __wfKnownArtifact(value) {
  if (value !== null && typeof value === "object") return __wfObjectArtifacts.get(value);
  return undefined;
}

function __wfDescribeInputValue(value, depth, path, state) {
  if (depth > __wfInputRecursionGuard) {
    throw new RangeError("use a flatter structured value for workflow inputs");
  }
  const artifact = __wfKnownArtifact(value);
  if (artifact !== undefined) {
    state.artifacts.push({ path: [...path], reference: artifact });
    return null;
  }
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value === "boolean") {
    return value;
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError("workflow values must contain finite numbers");
    if (Object.is(value, -0)) state.negativeZeros.push([...path]);
    return value;
  }
  if (typeof value === "string") {
    return value;
  }
  if (typeof value !== "object") throw new TypeError("workflow values must be JSON-compatible");
  if (state.active.has(value)) throw new TypeError("workflow values must be acyclic JSON-compatible values");
  state.active.add(value);
  let described;
  if (Array.isArray(value)) {
    described = [];
    for (let index = 0; index < value.length; index++) {
      path.push(index);
      described.push(__wfDescribeInputValue(value[index], depth + 1, path, state));
      path.pop();
    }
  } else {
    described = {};
    for (const key in value) {
      if (!Object.prototype.hasOwnProperty.call(value, key)) continue;
      path.push(key);
      const child = __wfDescribeInputValue(value[key], depth + 1, path, state);
      path.pop();
      Object.defineProperty(described, key, {
        value: child,
        enumerable: true,
        writable: true,
        configurable: true,
      });
    }
  }
  state.active.delete(value);
  return described;
}

async function __wfPrepareAgentInputs(options) {
  if (options.inputs === undefined) return options;
  const references = {};
  const preparedObjects = new WeakMap();
  for (const [alias, value] of Object.entries(options.inputs)) {
    let reference = __wfKnownArtifact(value);
    if (reference === undefined && value !== null && typeof value === "object") {
      reference = preparedObjects.get(value);
    }
    if (reference !== undefined) {
      Object.defineProperty(references, alias, {
        value: reference,
        enumerable: true,
        writable: true,
        configurable: true,
      });
      continue;
    }
    const state = {
      active: new WeakSet(),
      artifacts: [],
      negativeZeros: [],
    };
    const descriptor = {
      value: __wfDescribeInputValue(value, 1, [], state),
      artifacts: state.artifacts,
      negativeZeros: state.negativeZeros,
    };
    reference = await __wfHostInputArtifact({ descriptor });
    if (value !== null && typeof value === "object") {
      preparedObjects.set(value, reference);
    }
    Object.defineProperty(references, alias, {
      value: reference,
      enumerable: true,
      writable: true,
      configurable: true,
    });
  }
  return { ...options, inputs: references };
}

function __wfHostAgentOptions(options) {
  const withoutInputs = { ...options };
  const inputs = withoutInputs.inputs;
  delete withoutInputs.inputs;
  const sanitized = __wfSanitize(withoutInputs, __wfValueSanitizeLimits);
  if (inputs !== undefined) sanitized.inputs = inputs;
  return sanitized;
}

function __wfEnsureProgressText(field, value) {
  if (typeof value !== "string") throw new TypeError(`${field} must be a string`);
  if (__wfUtf8ByteLength(value) > __wfMaxProgressTextBytes) {
    throw new RangeError(`use a concise ${field}`);
  }
}

function __wfBuildApi() {
  function __wfInvokeAgent(apiName, prompt, options, resultMode) {
    const promise = (async () => {
    if (typeof prompt !== "string" || prompt.trim().length === 0) {
      throw new TypeError(`${apiName}() expects a non-empty string prompt`);
    }
    if (options === null || typeof options !== "object" || Array.isArray(options)) {
      throw new TypeError(`${apiName}() options must be an object`);
    }
    for (const [field, value] of [
      ["workflow agent label", options.label],
      ["workflow phase title", options.phase],
    ]) {
      if (value !== undefined) __wfEnsureProgressText(field, value);
    }
    let effectiveOptions = __wfAgentOptions(options);
    if (__wfChildMode) {
      effectiveOptions.phase = __wfPhaseTitle;
    } else if (effectiveOptions.phase === undefined && __wfPhaseTitle !== undefined) {
      effectiveOptions.phase = __wfPhaseTitle;
    }
    const phaseTitle = effectiveOptions.phase;
    const phaseIndex = __wfChildMode
      ? __wfPhaseIndex
      : __wfResolvePhase(phaseTitle, true);
    const invocation = __wfAgentInvocation();
    effectiveOptions = await __wfPrepareAgentInputs(effectiveOptions);
    let response;
    try {
      response = await __wfHostAgent({
        index: invocation.index,
        invocationId: invocation.invocationId,
        prompt,
        options: __wfHostAgentOptions(effectiveOptions),
        phaseIndex: phaseIndex ?? null,
        phaseTitle: phaseTitle ?? null,
        resultMode,
      });
    } catch (error) {
      const message = __wfErrorText(error);
      throw new Error(message);
    }
    __wfRunTokens += response.tokens || 0;
    const value = __wfDeepFreeze(response.value);
    __wfRememberArtifact(value, response.artifact);
      return value;
    })();
    return promise;
  }

  function agent(prompt, options = {}) {
    return __wfInvokeAgent("agent", prompt, options, "value");
  }

  function agentSettled(prompt, options = {}) {
    return __wfInvokeAgent("agentSettled", prompt, options, "settled");
  }

  async function parallel(thunks, options = {}) {
    if (!Array.isArray(thunks)) throw new TypeError("parallel() expects an array of functions");
    if (thunks.length > __wfMaxItems) {
      throw new RangeError("pass a focused work set to parallel(); split larger work across additional calls");
    }
    if (options === null || typeof options !== "object" || Array.isArray(options)) {
      throw new TypeError("parallel() options must be an object");
    }
    for (const key of Object.keys(options)) {
      if (key !== "requireAll") throw new TypeError(`parallel() received unknown option ${key}`);
    }
    if (options.requireAll !== undefined && typeof options.requireAll !== "boolean") {
      throw new TypeError("parallel() requireAll must be a boolean");
    }
    for (const thunk of thunks) {
      if (typeof thunk !== "function") {
        throw new TypeError("parallel() expects functions; wrap calls as () => agent(...)");
      }
    }
    const group = __wfAllocateInvocationGroup("parallel");
    const scopes = thunks.map((_, index) => __wfReserveAgentScope(`${group}/item:${index}`));
    const settled = await Promise.allSettled(thunks.map((thunk, index) =>
      Promise.resolve().then(() => __wfWithInvocationScope(scopes[index], thunk))
    ));
    const failures = settled
      .map((entry, index) => entry.status === "rejected" ? { index, reason: entry.reason } : null)
      .filter(Boolean);
    if (options.requireAll && failures.length > 0) {
      const details = failures.map(({ index, reason }) =>
        `[${index}] ${__wfTruncateUtf8(__wfErrorText(reason), 128, "...")}`
      ).join("; ");
      const message = __wfTruncateUtf8(
        `WorkflowParallelError: parallel(requireAll) failed for ${failures.length}/${thunks.length} items: ${details}`,
        __wfMaxParallelErrorBytes,
        "...",
      );
      throw __wfNamedError("WorkflowParallelError", message);
    }
    return settled.map((entry, index) => {
      if (entry.status === "fulfilled") return entry.value;
      log(__wfTruncateUtf8(`parallel[${index}] failed: ${__wfErrorText(entry.reason)}`, 256, "..."));
      return null;
    });
  }

  async function pipeline(items, ...stages) {
    if (!Array.isArray(items)) throw new TypeError("pipeline() expects an array as its first argument");
    if (items.length > __wfMaxItems) {
      throw new RangeError("pass a focused work set to pipeline(); split larger work across additional calls");
    }
    for (const stage of stages) {
      if (typeof stage !== "function") throw new TypeError("pipeline() stages must be functions");
    }
    const group = __wfAllocateInvocationGroup("pipeline");
    const scopes = stages.map((_, stageIndex) => items.map((_, itemIndex) =>
      __wfReserveAgentScope(`${group}/stage:${stageIndex}/item:${itemIndex}`)
    ));
    const settled = await Promise.allSettled(items.map(async (originalItem, index) => {
      let previous = originalItem;
      for (let stageIndex = 0; stageIndex < stages.length; stageIndex++) {
        if (previous === null) break;
        previous = await __wfWithInvocationScope(scopes[stageIndex][index], () =>
          stages[stageIndex](previous, originalItem, index)
        );
      }
      return previous;
    }));
    return settled.map((entry, index) => {
      if (entry.status === "fulfilled") return entry.value;
      log(`pipeline[${index}] failed: ${__wfErrorText(entry.reason)}`);
      return null;
    });
  }

  function phase(title) {
    if (typeof title !== "string" || title.trim().length === 0) {
      throw new TypeError("phase() expects a non-empty string");
    }
    __wfEnsureProgressText("workflow phase title", title);
    if (__wfChildMode) return;
    __wfPhaseIndex = __wfResolvePhase(title);
    __wfPhaseTitle = title;
    __wfNotify({ type: "workflow_phase", index: __wfPhaseIndex, title, kind: "active" });
  }

  function log(message) {
    __wfNotify({ type: "workflow_log", message: String(message) });
  }

  function workflow(nameOrRef, childArgs = null) {
    const promise = (async () => {
      if (__wfChildMode) {
        throw new Error("call workflow() from the root workflow");
      }
      let response;
      try {
        response = await __wfHostChild({
          invocationId: __wfChildInvocation(),
          nameOrRef: __wfSanitize(nameOrRef, __wfValueSanitizeLimits),
          args: __wfSanitize(childArgs, __wfValueSanitizeLimits),
          phaseIndex: __wfPhaseIndex ?? null,
          phaseTitle: __wfPhaseTitle ?? null,
        });
      } catch (error) {
        throw new Error(__wfErrorText(error));
      }
      __wfRunTokens += response.tokens || 0;
      const value = __wfDeepFreeze(response.value);
      __wfRememberArtifact(value, response.artifact);
      return value;
    })();
    return promise;
  }

  async function listInputs() {
    const files = await __wfHostDeclaredInput({ action: "list" });
    return __wfDeepFreeze(files);
  }

  async function readInput(path) {
    if (typeof path !== "string" || path.length === 0) {
      throw new TypeError("readInput() expects a non-empty workspace-relative path");
    }
    const file = await __wfHostDeclaredInput({ action: "read", path });
    return file.content;
  }

  return Object.freeze({
    agent,
    agentSettled,
    parallel,
    pipeline,
    phase,
    log,
    workflow,
    listInputs,
    readInput,
  });
}

const __wfOriginalDate = Date;
function __wfDate(...values) {
  if (!new.target) throw new Error("construct dates with `new Date(explicitValue)`");
  if (values.length === 0) throw new Error("construct dates with an explicit argument");
  return Reflect.construct(__wfOriginalDate, values, __wfOriginalDate);
}
Object.defineProperties(__wfDate, {
  now: { value: () => { throw new Error("provide the current time through workflow args"); } },
  parse: { value: __wfOriginalDate.parse },
  UTC: { value: __wfOriginalDate.UTC },
  prototype: { value: __wfOriginalDate.prototype },
});
Object.defineProperty(__wfOriginalDate.prototype, "constructor", {
  value: __wfDate,
  writable: false,
  configurable: false,
});
Object.freeze(__wfOriginalDate.prototype);
Object.defineProperty(globalThis, "Date", {
  value: Object.freeze(__wfDate),
  writable: false,
  configurable: false,
});
Object.defineProperty(Math, "random", {
  value: () => { throw new Error("provide random values through workflow args"); },
});

for (const name of [
  "ShadowRealm",
  "WebAssembly",
  "FinalizationRegistry",
  "WeakRef",
  "Atomics",
  "SharedArrayBuffer",
  "Temporal",
  "queueMicrotask",
  "$vm",
]) {
  try { delete globalThis[name]; } catch (_) { globalThis[name] = undefined; }
}
Object.defineProperty(globalThis, "then", { value: undefined, writable: false, configurable: false });

function __wfFreezePrototypeChain(value) {
  let prototype = Object.getPrototypeOf(value);
  while (prototype && prototype !== Object.prototype && prototype !== Function.prototype) {
    Object.freeze(prototype);
    prototype = Object.getPrototypeOf(prototype);
  }
}

for (const value of [
  [][Symbol.iterator](),
  new Map()[Symbol.iterator](),
  new Set()[Symbol.iterator](),
  ""[Symbol.iterator](),
  (function* () {})(),
  (async function* () {})(),
  function* () {},
  async function () {},
  async function* () {},
  Uint8Array.prototype,
]) {
  __wfFreezePrototypeChain(value);
}

for (const value of [
  Object.getPrototypeOf(function* () {}).constructor,
  Object.getPrototypeOf(async function () {}).constructor,
  Object.getPrototypeOf(async function* () {}).constructor,
  Object.getPrototypeOf(Uint8Array),
]) {
  if (value && value.prototype) Object.freeze(value.prototype);
  if (value) Object.freeze(value);
}

for (const name of Object.getOwnPropertyNames(Intl)) {
  const value = Intl[name];
  if (typeof value !== "function") continue;
  if (value.prototype) Object.freeze(value.prototype);
  Object.freeze(value);
}

for (const value of [
  Object, Function, Array, Number, BigInt, Boolean, String, RegExp, Error, AggregateError,
  EvalError, RangeError, ReferenceError, SyntaxError, TypeError, URIError, SuppressedError,
  Map, Set, WeakMap, WeakSet, Promise, Symbol, ArrayBuffer, DataView, DisposableStack,
  AsyncDisposableStack, Iterator, Uint8Array, Uint8ClampedArray, Uint16Array, Uint32Array,
  Int8Array, Int16Array, Int32Array, Float16Array, Float32Array, Float64Array,
  BigInt64Array, BigUint64Array,
]) {
  if (value && value.prototype) Object.freeze(value.prototype);
  if (value) Object.freeze(value);
}
for (const value of [JSON, Math, Reflect, Proxy, Intl]) Object.freeze(value);
