# Workflow Script API

## Invocation

Workflow accepts one of these sources:

- `script`: inline JavaScript for a new custom workflow.
- `name`: a built-in, plugin, user, or project workflow. Plugin names use
  `pluginName:workflowName`.
- `scriptPath`: a persisted script from an earlier invocation. It takes precedence over `script`,
  which takes precedence over `name`.

Pass `args` as a JSON value. The script receives that value unchanged through the global `args`.
Every resolved script is persisted, and the launch result includes its `scriptPath` and `runId`.

## Script Format

Begin with a literal metadata declaration:

```javascript
export const meta = {
  name: "inspect-and-summarize",
  description: "Inspect several areas and synthesize the findings",
  title: "Inspect and summarize",
  phases: [
    { title: "Inspect", detail: "Explore independent areas" },
    { title: "Synthesize", detail: "Combine the findings" },
  ],
};
```

`name` and `description` are required. `title`, `whenToUse`, and `phases` are optional. Metadata is
a pure object literal. Use phase titles consistently between `meta.phases`, `phase()`, and agent
options.

The body is JavaScript in an async context, so top-level `await` is available. Finish with
`return value`; the value must be JSON-compatible. The runtime supplies its API as globals rather
than Node.js modules. Dynamic imports and string code generation are unavailable, and values are
sanitized across host boundaries. Time and randomness used for deterministic resume should enter
through `args`; dates constructed from explicit values are supported, while ambient clocks and
randomness are excluded.

## Runtime Globals

### `agent(prompt, options?)`

Runs one subagent. `prompt` is one non-empty string and is passed through verbatim. Use a template
literal or `.join(...)` when composing it from multiple parts.

Options:

- `label`: short progress label.
- `phase`: explicit phase title, especially useful in concurrent work.
- `schema`: JSON Schema for a validated structured result.
- `model`: model override; omission inherits the resolved parent model.
- `effort`: `low`, `medium`, `high`, `xhigh`, or `max`.
- `isolation`: `worktree` for a temporary git worktree when parallel agents mutate files.
- `agentType`: registered custom agent type.
- `stallMs`: no-progress timeout, capped at 1,800,000 ms; the default is 180,000 ms.

Without `schema`, the result is final text. With `schema`, it is the validated object. A skipped or
terminally failed agent yields `null`, so filter nullable values before using them. Workflow agents
inherit the parent's ordinary tools and connected MCP tools; orchestration and user messaging stay
with the owning agent, and each subagent's final response becomes the function result. Stalled
calls retry up to three times with exponential backoff, then pause for retry or skip.

### `pipeline(items, ...stages)`

Moves every item through all stages independently. Each stage receives
`(previousResult, originalItem, index)`. A failed stage yields `null` for that item while other
items continue.

```javascript
const reviewed = await pipeline(
  args.dimensions,
  (dimension) => agent(`Inspect ${dimension.name}`, {
    label: `inspect:${dimension.name}`,
    phase: "Inspect",
    schema: FINDINGS_SCHEMA,
  }),
  (found, dimension) => agent(
    `Verify ${dimension.name}:\n\n${JSON.stringify(found)}`,
    { label: `verify:${dimension.name}`, phase: "Verify", schema: VERDICTS_SCHEMA },
  ),
);
```

### `parallel(thunks)`

Runs functions concurrently and waits for all of them. Each failed thunk yields `null`; siblings
continue.

```javascript
const reports = await parallel(args.areas.map((area, index) => () =>
  agent(`Inspect ${area}`, { label: `inspect:${index}`, phase: "Inspect" })
));

const material = reports.filter(Boolean).join("\n\n");
return agent(`Synthesize these reports:\n\n${material}`, {
  label: "synthesize",
  phase: "Synthesize",
});
```

### Progress and budget

- `phase(title)` activates a progress group.
- `log(message)` emits a bounded progress message.
- `budget.total` is the owning turn's token ceiling or `null`.
- `budget.spent()` returns current shared usage.
- `budget.remaining()` returns the remaining tokens or `Infinity` without a ceiling.

Inside concurrent stages, set the agent `phase` option explicitly because the global active phase
is shared. Reaching a configured token ceiling blocks new agent calls while in-flight calls settle.

Guard budget-driven loops with `budget.total`:

```javascript
while (budget.total && budget.remaining() > 50_000) {
  // Run another bounded round.
}
```

### `workflow(nameOrRef, childArgs?)`

Runs a saved child workflow inline. `nameOrRef` accepts a workflow name, `{name: "..."}`, or
`{scriptPath: "..."}`. The child receives `childArgs`, shares the parent concurrency, cancellation,
journal, and token budget, and is grouped under the parent phase. One nesting level is supported.

## Limits and Recovery

Concurrent agents are limited to `min(16, max(2, CPU cores - 2))`; additional calls queue. A run
supports up to 1,000 agents, each `parallel()` or `pipeline()` call up to 4,096 items, scripts up to
512 KiB, and child workflows up to the configured limit of 16 by default.

The helpers settle every item rather than failing fast. Budget exhaustion inside a helper yields a
`null` slot. Keep synthesis inputs bounded by reducing or ranking fan-out results first, and report
intentional sampling through `log()`.

After a run stops or fails, invoke Workflow with `scriptPath`, `resumeFromRunId`, and any required
`args`. The journal replays the longest unchanged prefix of agent calls. Cache identity includes
the executable script, prompt, `schema`, `model`, `effort`, `isolation`, and `agentType`; changing
executable code starts a new cache chain, while labels, phases, and stall timeouts do not.
