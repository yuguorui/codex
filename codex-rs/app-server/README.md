# Thread removal

`thread/archive` and `thread/delete` reject attempts to remove a live internal
worker with JSON-RPC error `-32600`. The worker's owner controls its shutdown.
For example, a Guardian reviewer remains available to its parent conversation
after a client tries to archive or delete it.

After the owner releases the worker, its saved conversation can be archived or
deleted normally. Ordinary client-controlled threads keep their existing behavior.

## Dynamic workflows (Codex++ fork extensions)

The Codex++ fork adds background dynamic-workflow runs driven by the model-facing
`Workflow` tool. All workflow RPCs below are experimental and require
`capabilities.experimentalApi`.

- `workflow/list` — experimental; page background dynamic-workflow runs for a loaded thread. Returns task snapshots with the latest bounded phase, agent, and log window, plus usage, terminal outcome counts, failures, and output paths.
- `workflowApprovalArtifact/read` — experimental; read and verify a bounded page of a Workflow approval action by `threadId`, content-addressed `artifactId`, and optional byte `offset`. Returns `{ sha256, offset, contents, nextOffset }`; continue from `nextOffset` until it is `null` to inspect the complete action without filesystem access to the app-server host.
- `workflow/stop` — experimental; request cancellation of an active workflow by `threadId` and `runId`. Returns `accepted: false` when the run is already terminal.
- `workflow/skipAgent` — experimental; stop the active attempt for one workflow agent and settle that slot as skipped.
- `workflow/retryAgent` — experimental; stop the active attempt for one workflow agent and schedule another attempt.

### Example: Inspect and control dynamic workflows

Dynamic workflows are model tools, not a separate launch RPC. Enable the `workflows` feature, initialize with `capabilities.experimentalApi: true`, and start a normal turn in which the user explicitly asks to run a workflow. The `Workflow` launch response marks `transcriptDir` and `scriptPath` as `appServerHostArtifact`; they are persisted host paths, not paths in the selected remote execution environment. After any required tool approval, the `Workflow` tool returns immediately while execution continues in the background. A resume call may omit `args` to reuse the terminal run's persisted arguments; explicit resume arguments replace them and disable journal replay. When the owning model needs the result before continuing its current turn, it can call `WaitWorkflow` with the returned `runId`; that tool waits for a terminal state or its configured timeout and returns a focused terminal result inline when available. When `resultTruncated` is true or `resultError` is non-null, the model reads the result with `ReadWorkflowResult`, using the same `runId`, starting at offset `0`, and continuing from each `nextOffset`. For a result that should not be paged through model context, the model may instead pass `ReadWorkflowResult` a `writePath` relative to the primary selected execution environment cwd (or absolute inside one of its workspace roots); Codex writes the complete verified JSON result through that environment's filesystem and returns only bounded metadata. `ReadWorkflowResult` also accepts an RFC 6901 `jsonPointer` no longer than 512 UTF-8 bytes; it returns the selected value directly when bounded, or writes exactly that projected JSON when combined with `writePath`. Projection cannot be combined with `offset` or `maxBytes`. `AnalyzeWorkflowInputs` returns a bounded `resultShape` and `nextAction` when a program result is too large, so the model can narrow its next program instead of receiving a generic error. `WaitWorkflow` accepts the same `writePath` option after its terminal wait, so a critical-path result can be written without a separate read call. The connection receives `workflow/started`, zero or more `workflow/progress` snapshots, and one terminal `workflow/completed` notification.

Workflow agent execution is independent of the model-visible multi-agent protocol. Agent v1 parent turns retain the `multi_agent_v1` tool namespace, Agent v2 parent turns retain the `collaboration` namespace, and both can launch the same `Workflow` tool and DSL. Workflow-owned child agents expose neither multi-agent namespace nor `Workflow`, which prevents nested orchestration without requiring a v2-only call path.

Structured workflow agents use the provider's native strict JSON Schema output on OpenAI providers. Other providers receive the same bounded schema in the child-agent prompt and are validated locally, so Chat, Anthropic, and open-model providers do not need to implement the Responses `text.format` field.

Workflow child agents inherit the effective configuration and selected executor of the sampling step that launched the Workflow, including model/provider, instructions, service tier, approval policy, and reviewer. Keep child prompts to stable task instructions and pass variable data through `agent(..., {inputs})`. Agents with inputs receive `AnalyzeWorkflowInputs`, which provides programmatic access to the complete deep-frozen input object in fresh V8 isolates. Use `parallel(..., {requireAll: true})` so critical synthesis starts with every required result.

Workflow approvals include a content-addressed `codex://workflow-approval/<threadId>/<sha256>` reference. Remote clients can read the exact reviewed bytes while the approval is pending:

```json
{ "method": "workflowApprovalArtifact/read", "id": 37, "params": {
    "threadId": "11111111-1111-4111-8111-111111111111",
    "artifactId": "<sha256 from the approval reference>",
    "offset": 0
} }
```

Each response includes `sha256`, `offset`, bounded `contents`, and a nullable `nextOffset`. Read every page by passing the preceding `nextOffset`. The content binds the frozen Workflow definition and arguments together with every selected environment's location, cwd, workspace roots, environment configuration, sandbox context, effective approval policy, redacted child capabilities, and opaque executor ID. Codex verifies the persisted bytes again after approval and launches from the already approved in-memory definition, child configuration, project-instruction snapshot, and captured executor handles.

Use `workflow/list` to rebuild UI state after reconnecting or opening a workflow panel:

```json
{ "method": "workflow/list", "id": 38, "params": {
    "threadId": "thr_123",
    "cursor": null,
    "limit": 20
} }
{ "id": 38, "result": {
    "data": [
        {
            "threadId": "thr_123",
            "turnId": "turn_456",
            "taskId": "w4f91a02c",
            "runId": "wf_01abc234",
            "workflowName": "code-review",
            "title": null,
            "status": "running",
            "summary": "Running workflow code-review",
            "transcriptDir": "/path/to/subagents/workflows/wf_01abc234",
            "scriptPath": "/path/to/workflows/scripts/code-review-wf_01abc234.js",
            "outputFile": "/path/to/sessions/thr_123/workflows/wf_01abc234.json",
            "progress": [],
            "progressVersion": 0,
            "usage": {
              "totalTokens": 0,
              "toolUses": 0,
              "durationMs": 0,
              "agentCount": 0,
              "successfulAgentCount": 0,
              "failedAgentCount": 0,
              "skippedAgentCount": 0,
              "nullAgentResultCount": 0
            },
            "failures": [],
            "error": null,
            "startedAt": 1786200000,
            "completedAt": null
        }
    ],
    "nextCursor": null
} }
```

Stop a whole run, or control one active agent by its stable progress `index`:

```json
{ "method": "workflow/stop", "id": 39, "params": {
    "threadId": "thr_123",
    "runId": "wf_01abc234"
} }
{ "id": 39, "result": { "accepted": true } }

{ "method": "workflow/skipAgent", "id": 40, "params": {
    "threadId": "thr_123",
    "runId": "wf_01abc234",
    "agentIndex": 3
} }
{ "id": 40, "result": { "accepted": true } }

{ "method": "workflow/retryAgent", "id": 41, "params": {
    "threadId": "thr_123",
    "runId": "wf_01abc234",
    "agentIndex": 3
} }
{ "id": 41, "result": { "accepted": true } }
```

All four methods are experimental and require `capabilities.experimentalApi`. Workflow notifications are thread-scoped and are sent only to connections currently subscribed to the owning thread.

### Dynamic workflow events (experimental)

- `workflow/started` — identifies the background task and run and includes `threadId`, `turnId`, `taskId`, `runId`, `workflowName`, nullable `title`, `summary`, `transcriptDir`, `scriptPath`, stable `deliveryKey`, and Unix-second `startedAt`.
- `workflow/progress` — carries the latest bounded `progress` snapshot plus cumulative `usage`. Progress items are tagged as `workflowPhase`, `workflowAgent`, or `workflowLog`. Agent items expose stable `invocationId`, queue/running/terminal state, retry attempt, cache/skip/block flags, token and tool counts, timing, and bounded prompt/result previews.
- `workflow/completed` — terminal notification with `status` (`completed`, `failed`, `paused`, or `killed`), `summary`, `outputFile`, nullable `error`, partial `failures`, cumulative `usage` with terminal agent outcome counts (`successfulAgentCount`, `failedAgentCount`, `skippedAgentCount`, and `nullAgentResultCount`), stable `deliveryKey`, `progressResyncRequired`, and Unix-second `completedAt`. When `progressResyncRequired` is true, refresh the task with `workflow/list` before rendering final progress. `outputFile` points to the run snapshot; the terminal snapshot includes a content-addressed result artifact descriptor alongside progress and usage. Running snapshots are persisted at most once every two seconds, with the result artifact and final snapshot durably written before this notification.

Clients should deduplicate `workflow/started` and `workflow/completed` retries by `deliveryKey`. Delivery is tracked independently for each subscribed connection; notification opt-out and missing experimental capability mean that connection is not a target. A disconnected or stalled target remains retryable until a later online attempt writes successfully. Clients should key live agent rows by `(taskId, invocationId, index)` and replace prior snapshots rather than append them. A workflow can outlive the turn that launched it; `turn/completed` does not imply `workflow/completed`. Respect the client animation setting when rendering running-state animation. The built-in TUI uses a truecolor shimmer when supported (with a reduced-motion fallback), cyan for running agents, green for completed work, red for failures or blocked work, and dim styling for skipped or stopped work.
