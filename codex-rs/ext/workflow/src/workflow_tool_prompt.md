Execute a deterministic JavaScript workflow that coordinates multiple Codex subagents. Workflows
run in the background: this tool returns immediately with a task ID and run ID. Use `/workflows`
to watch progress and to stop a run, skip an agent, or retry an agent.

Workflow lifecycle events are delivered automatically and are authoritative. Do not poll
transcript files or workflow snapshots with shell commands or sleeps. After a completed, failed,
or stopped notification, stop monitoring that run.

A workflow is useful when work must be comprehensive, independently verified, or spread across
more context than one agent can hold. The script defines deterministic control flow: what fans
out, what advances independently, what waits at a barrier, and what synthesizes the result.

Only call this tool after explicit opt-in to multi-agent orchestration. Explicit opt-in includes:

- The user asks for a workflow, ultracode, fan-out agents, or multi-agent orchestration.
- The user names a specific built-in, plugin, or saved workflow.
- A system, developer, skill, or slash-command instruction explicitly requires Workflow.

Do not infer opt-in from an ordinary coding task, even when parallel agents might help. Workflows
can create many agents and consume substantial tokens. If orchestration would help but the user
did not opt in, explain the likely shape and cost briefly and ask first.

Prefer a hybrid workflow. Inspect enough context in the owning agent to identify a bounded work
list, then use Workflow for the fan-out. For larger work, run several focused workflows in
sequence and inspect each result before deciding the next phase.

## Invocation

For a new custom workflow, pass the script inline with `script`. Do not create a temporary script
file first. Every invocation persists the resolved script under the session directory and returns
its `scriptPath`. To iterate, edit that persisted file and invoke
`Workflow({scriptPath, resumeFromRunId})`.

Use `name` for a built-in or saved workflow. Saved `.js` workflows are discovered from active
plugin `workflows/` directories, user workflow directories, and project `.codex/workflows` or
compatibility `.claude/workflows` directories. A plugin workflow is addressed as
`pluginName:workflowName`. Deeper project definitions take precedence; the approval preview
warns when a file shadows a lower-priority definition.

`scriptPath` takes precedence over `script`, which takes precedence over `name`. Pass `args` as an
actual JSON value, not a JSON-encoded string. For example, use `args: ["a.rs", "b.rs"]`, not
`args: "[\"a.rs\",\"b.rs\"]"`. The exact JSON value is exposed as the global `args`.

Every script must begin with a pure literal metadata declaration:

```javascript
export const meta = {
  name: "review-changes",
  description: "Review changed files and independently verify findings",
  title: "Review changes",
  whenToUse: "When a user explicitly requests a multi-agent code review",
  phases: [
    { title: "Find", detail: "Review independent dimensions" },
    { title: "Verify", detail: "Try to refute each finding", model: "optional-model" },
  ],
};

phase("Find");
// Script body continues here.
```

`meta` must be a pure object literal. Variables, calls, spreads, computed values, and template
interpolation are forbidden. `name` and `description` are required. `title`, `whenToUse`, and
`phases` are optional. Use the same phase titles in `meta.phases` and `phase()` calls. The
compatibility `description` and `title` fields on the tool input are ignored; put them in `meta`.

Scripts are JavaScript, not TypeScript. The body is already in an async context, so use `await`
directly. There is no filesystem, process, network, or other Node.js API. `import()` is unavailable.
String code generation through `eval` or `Function` is disabled. The runtime freezes its exposed
intrinsics and sanitizes values crossing every host boundary.

`Date.now()`, argument-free `new Date()`, `Date()`, and `Math.random()` are unavailable because
they break deterministic resume. A date constructed from an explicit value, such as
`new Date(args.timestamp)`, is allowed. Pass time or seed values through `args`, or add timestamps
after the workflow returns.

## Runtime API

The script has these globals:

### agent(prompt, options?)

Spawn one workflow subagent. `prompt` must be a non-empty string. Supported options are:

- `label`: short progress label.
- `phase`: explicit phase title. Set this inside concurrent stages to avoid relying on mutable
  global phase state.
- `schema`: JSON Schema for a validated structured result. The returned value is the validated
  object, so do not parse model text yourself.
- `model`: model override. Omit it to inherit the resolved parent model.
- `effort`: `low`, `medium`, `high`, `xhigh`, or `max`. Omit it to inherit parent effort.
- `isolation`: `worktree` for a fresh git worktree. Use only for agents that mutate files in
  parallel. The worktree is temporary on successful completion, so the agent must return every
  needed result or patch. Remote isolation is not available in this build.
- `agentType`: registered custom agent type. It still cannot spawn agents or Workflow.
- `stallMs`: no-progress timeout, capped at 1,800,000 ms. The default is 180,000 ms.

Without `schema`, `agent()` returns final text. With `schema`, it returns a validated JSON value.
The host uses native structured output when the provider supports it and a bounded validated
fallback otherwise. Schema-correction turns stay in the same subagent conversation, so the prior
output remains available without being copied into a fresh prompt. A skipped agent or terminal API
failure returns `null`; a stalled agent is retried up to five times before the call throws. Filter
nullable results before dereferencing them.

The workflow layer passes `prompt` through verbatim: it does not truncate it or impose a separate
byte limit. The selected model's normal context-window behavior still applies.
Keep aggregate prompts bounded in the script by reducing, ranking, or summarizing fan-out results
before synthesis; do not blindly concatenate unrestricted agent outputs.

Workflow subagents receive the parent session's ordinary tools and connected MCP tools, but they
cannot use Agent, either Agent v1 or v2 orchestration namespace, Workflow, or user-messaging tools.
Their final response is a value for the script, not a message to the user.

### pipeline(items, ...stages)

Run every item through all stages independently. There is no barrier between stages: item A can
enter stage 3 while item B is still in stage 1. This is the default for multi-stage work. Each
stage receives `(previousResult, originalItem, index)`. A stage that throws turns that item into
`null`, logs the failure, and skips its remaining stages. Other items continue.

```javascript
const reviewed = await pipeline(
  dimensions,
  (dimension) => agent(dimension.prompt, {
    label: `find:${dimension.name}`,
    phase: "Find",
    schema: FINDINGS_SCHEMA,
  }),
  (found, dimension) => agent(
    `Try to refute every finding from ${dimension.name}: ${JSON.stringify(found)}`,
    { label: `verify:${dimension.name}`, phase: "Verify", schema: VERDICTS_SCHEMA },
  ),
);
```

### parallel(thunks)

Run functions concurrently and wait for all of them. This is an explicit barrier. Each thrown
thunk becomes `null` and is logged; the other thunks continue and the call itself does not fail
fast.

```javascript
const results = await parallel(items.map((item, index) => () =>
  agent(`Inspect ${item}`, { label: `inspect:${index}`, phase: "Inspect" })
));
const successful = results.filter(Boolean);
```

Use a barrier only when the next step needs cross-item context from all prior results, such as
global deduplication, aggregate ranking, or an early exit based on the total result count. A plain
map/filter/flatten between stages is not a reason for a barrier; put that transformation in a
pipeline stage. Default to `pipeline()` when in doubt.

### phase(title) and log(message)

`phase(title)` activates a progress group for subsequent calls. Prefer the explicit `phase` agent
option inside `pipeline()` and `parallel()` because concurrent callbacks can otherwise race on the
global phase. `log(message)` emits a bounded narrator line in `/workflows`.

### budget

`budget` is `{ total, spent(), remaining() }`. `total` is the owning turn's hard token ceiling, or
`null` when no ceiling exists. `spent()` reads the live shared usage, and `remaining()` returns
`max(0, total - spent())` or `Infinity` without a ceiling. Once the ceiling is reached, new
`agent()` calls throw `WorkflowBudgetExceededError`; calls already in flight are allowed to settle.

Always guard budget-driven loops with `budget.total`, because `remaining()` is infinite without a
configured ceiling:

```javascript
const findings = [];
while (budget.total && budget.remaining() > 50_000) {
  const next = await agent("Find a new issue not already listed", { schema: FINDINGS_SCHEMA });
  if (next) findings.push(...next.findings);
  log(`${findings.length} findings; ${Math.round(budget.remaining() / 1000)}k tokens remain`);
}
```

### workflow(nameOrRef, childArgs?)

Run a saved child workflow inline and return its result. `nameOrRef` may be a saved workflow name,
`{name: "..."}`, or `{scriptPath: "..."}`. The child receives `childArgs` as its `args`, shares the
parent concurrency limiter, agent count, cancellation signal, journal, and token budget, and is
grouped under the parent phase. Nesting is one level only: `workflow()` inside a child throws.
At most 16 child sessions are created by default; users may configure a value from 1 through 64.

## Concurrency And Failure Semantics

Concurrent agents are limited to `min(16, max(2, CPU cores - 2))`; excess calls queue. A run may
create at most 1,000 agents. One `parallel()` or `pipeline()` call accepts at most 4,096 items.
Script size is capped at 512 KiB. These are hard errors, not silent truncation.

`parallel()` and `pipeline()` are all-settled and never fail fast. A single agent failure does not
cancel its phase or sibling items. Budget exhaustion inside these helpers produces a `null` slot
and a log entry. Outside the helpers, catch errors when the script can recover deliberately.

Do not silently cap coverage. If the workflow samples, keeps only a top N, or drops work because
of budget, call `log()` with what was omitted. Otherwise a partial sweep can look complete.

## Quality Patterns

Choose the verification shape that matches the user's requested confidence:

- Adversarial verification: ask independent agents to refute each candidate, then keep only claims
  that survive the required vote threshold.
- Perspective-diverse verification: use distinct correctness, security, performance, and
  reproducibility lenses instead of identical judges.
- Judge panel: generate independent approaches, score them, and synthesize from the strongest
  proposal while retaining useful parts of the others.
- Loop until dry: continue discovery until several consecutive rounds produce no new items. Dedup
  against every item already seen, not only confirmed items, to prevent rejected candidates from
  returning forever.
- Multi-modal sweep: search by different containers, content patterns, entities, time ranges, or
  code paths, then merge evidence.
- Completeness critic: use a final agent to identify missing files, modalities, unverified claims,
  unread sources, or untested paths. Feed concrete gaps into another bounded round.

For a quick request, use a small finder set and light verification. For a comprehensive audit or
research request, use more independent finders, three to five adversarial votes, and a final
completeness pass. Keep deterministic transformations such as grouping, deduplication, sorting,
and thresholding in JavaScript rather than asking another model to perform them.

## Resume

The result includes `runId`, `scriptPath`, and `transcriptDir`. Stop a still-running workflow before
resuming it. Relaunch with `Workflow({scriptPath, resumeFromRunId, args})`; the resumed task keeps
the same run ID and the newly resolved script is reviewed again when approval is required.

Agent cache keys form a rolling call chain rooted in the approved executable script body, then each
prompt and the selected semantic options: `schema`, `model`, `effort`, `isolation`, and `agentType`.
Labels, phases, and `stallMs` do not alter the result cache. With the same script body, the longest
unchanged invocation prefix is replayed immediately. The first changed, missing, reordered, or
unfinished call disables replay for that call and everything after it. Changing executable script
code invalidates the prior chain; metadata-only edits do not.

After a terminal notification, inspect `<transcriptDir>/journal.jsonl` once before diagnosing an
empty or unexpected result. It records `started` and full validated `result` entries; a `started`
entry does not mean the run is still active, and a cached result may legitimately be empty. If the
approved script changed on disk while Codex was not running, automatic adoption pauses the run and
requires an explicit Workflow invocation so the changed script can be reviewed again.
