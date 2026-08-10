const __wfMaxItems = 4096;
const __wfMaxTimers = 64;
const __wfMaxProgressTextBytes = 256;
let __wfNextAgentIndex = 0;
let __wfRunTokens = 0;
let __wfBudgetSpentTokens = __wfInitialSpentTokens;
let __wfPhaseIndex = __wfInitialPhaseIndex ?? undefined;
let __wfPhaseTitle = __wfInitialPhaseTitle ?? undefined;
const __wfHostNotify = globalThis.notify;
const __wfHostAgent = globalThis.tools.workflow_agent;
const __wfHostChild = globalThis.tools.workflow_child;
const __wfHostResult = globalThis.tools[__wfResultToolName];
const __wfHostSetTimeout = globalThis.setTimeout;
const __wfHostClearTimeout = globalThis.clearTimeout;
const __wfActiveTimers = new Set();

function __wfSetTimeout(callback, delay = 0) {
  if (typeof callback !== "function") throw new TypeError("setTimeout expects a function callback");
  if (__wfActiveTimers.size >= __wfMaxTimers) {
    throw new RangeError("workflow supports at most 64 active timers");
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

function __wfSanitize(value, seen = new WeakSet()) {
  if (value === undefined) return null;
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError("workflow values must contain finite numbers");
    return value;
  }
  if (typeof value !== "object") throw new TypeError("workflow values must be JSON-compatible");
  if (seen.has(value)) throw new TypeError("workflow values must not contain cycles");
  seen.add(value);
  if (Array.isArray(value)) {
    if (value.length > __wfMaxItems) throw new RangeError("workflow array exceeds 4096 items");
    const result = value.map(item => __wfSanitize(item, seen));
    seen.delete(value);
    return result;
  }
  const result = {};
  const entries = Object.entries(value);
  if (entries.length > __wfMaxItems) throw new RangeError("workflow object exceeds 4096 properties");
  for (const [key, item] of entries) result[key] = __wfSanitize(item, seen);
  seen.delete(value);
  return result;
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

function __wfEnsureProgressText(field, value) {
  if (typeof value !== "string") throw new TypeError(`${field} must be a string`);
  if (__wfUtf8ByteLength(value) > __wfMaxProgressTextBytes) {
    throw new RangeError(`${field} exceeds the ${__wfMaxProgressTextBytes}-byte limit`);
  }
}

function __wfBuildApi() {
  const budget = Object.freeze({
    total: __wfTokenBudget,
    spent: () => __wfBudgetSpentTokens,
    remaining: () => __wfTokenBudget === null
      ? Number.POSITIVE_INFINITY
      : Math.max(0, __wfTokenBudget - __wfBudgetSpentTokens),
  });

  async function agent(prompt, options = {}) {
    if (typeof prompt !== "string" || prompt.trim().length === 0) {
      throw new TypeError("agent() expects a non-empty string prompt");
    }
    if (options === null || typeof options !== "object" || Array.isArray(options)) {
      throw new TypeError("agent() options must be an object");
    }
    for (const [field, value] of [
      ["workflow agent label", options.label],
      ["workflow phase title", options.phase],
    ]) {
      if (value !== undefined) __wfEnsureProgressText(field, value);
    }
    if (budget.remaining() <= 0) {
      throw __wfNamedError("WorkflowBudgetExceededError", "workflow token budget exceeded");
    }
    const index = __wfNextAgentIndex++;
    const effectiveOptions = { ...options };
    if (__wfChildMode) {
      effectiveOptions.phase = __wfPhaseTitle;
    } else if (effectiveOptions.phase === undefined && __wfPhaseTitle !== undefined) {
      effectiveOptions.phase = __wfPhaseTitle;
    }
    const phaseTitle = effectiveOptions.phase;
    const phaseIndex = __wfChildMode
      ? __wfPhaseIndex
      : __wfResolvePhase(phaseTitle, true);
    let response;
    try {
      response = await __wfHostAgent({
        index,
        prompt,
        options: __wfSanitize(effectiveOptions),
        phaseIndex: phaseIndex ?? null,
        phaseTitle: phaseTitle ?? null,
      });
    } catch (error) {
      const message = __wfErrorText(error);
      if (message.includes("WorkflowBudgetExceededError")) {
        throw __wfNamedError("WorkflowBudgetExceededError", message);
      }
      if (message.includes("WorkflowAgentCapError")) {
        throw __wfNamedError("WorkflowAgentCapError", message);
      }
      throw new Error(message);
    }
    __wfRunTokens += response.tokens || 0;
    __wfBudgetSpentTokens = response.spent ?? (__wfBudgetSpentTokens + (response.tokens || 0));
    return response.value;
  }

  async function parallel(thunks) {
    if (!Array.isArray(thunks)) throw new TypeError("parallel() expects an array of functions");
    if (thunks.length > __wfMaxItems) throw new RangeError("parallel() accepts at most 4096 items");
    for (const thunk of thunks) {
      if (typeof thunk !== "function") {
        throw new TypeError("parallel() expects functions; wrap calls as () => agent(...)");
      }
    }
    const settled = await Promise.allSettled(thunks.map(thunk => Promise.resolve().then(thunk)));
    return settled.map((entry, index) => {
      if (entry.status === "fulfilled") return entry.value;
      log(`parallel[${index}] failed: ${__wfErrorText(entry.reason)}`);
      return null;
    });
  }

  async function pipeline(items, ...stages) {
    if (!Array.isArray(items)) throw new TypeError("pipeline() expects an array as its first argument");
    if (items.length > __wfMaxItems) throw new RangeError("pipeline() accepts at most 4096 items");
    for (const stage of stages) {
      if (typeof stage !== "function") throw new TypeError("pipeline() stages must be functions");
    }
    const settled = await Promise.allSettled(items.map(async (originalItem, index) => {
      let previous = originalItem;
      for (const stage of stages) {
        if (previous === null) break;
        previous = await stage(previous, originalItem, index);
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

  async function workflow(nameOrRef, childArgs = null) {
    if (__wfChildMode) {
      throw new Error("workflow() cannot be called from within a child workflow; nesting is limited to one level");
    }
    let response;
    try {
      response = await __wfHostChild({
        nameOrRef: __wfSanitize(nameOrRef),
        args: __wfSanitize(childArgs),
        phaseIndex: __wfPhaseIndex ?? null,
        phaseTitle: __wfPhaseTitle ?? null,
      });
    } catch (error) {
      throw new Error(__wfErrorText(error));
    }
    __wfRunTokens += response.tokens || 0;
    __wfBudgetSpentTokens = response.spent ?? (__wfBudgetSpentTokens + (response.tokens || 0));
    return response.value;
  }

  return Object.freeze({ agent, parallel, pipeline, phase, log, budget, workflow });
}

const __wfOriginalDate = Date;
function __wfDate(...values) {
  if (!new.target) throw new Error("Date() is nondeterministic in workflows");
  if (values.length === 0) throw new Error("new Date() without an argument is nondeterministic in workflows");
  return Reflect.construct(__wfOriginalDate, values, __wfOriginalDate);
}
Object.defineProperties(__wfDate, {
  now: { value: () => { throw new Error("Date.now() is nondeterministic in workflows"); } },
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
  value: () => { throw new Error("Math.random() is nondeterministic in workflows"); },
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
