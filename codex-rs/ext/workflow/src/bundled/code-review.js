export const meta = {
  name: "code-review",
  description: "Workflow-backed code review \u2014 one finder per correctness angle plus one finder covering all cleanup angles, an independent verifier for every distinct (file, line) location across the pooled candidates, then a ranked, capped findings report.",
  whenToUse: "Launched by the /code-review skill at high, xhigh, or max effort when workflows are enabled. Pass args as \"<level> [target]\" \u2014 level is high, xhigh, or max; target is an optional PR number, branch, ref range, path, or free-form review instructions (e.g. \"only review src/foo.ts\", \"focus on error handling\").",
  phases: [{"title": "Scope", "detail": "Pin the diff command, changed files, applicable AGENTS.md files, and conventions"}, {"title": "Find", "detail": "One finder per correctness angle plus one finder covering all cleanup angles, pooled before verify"}, {"title": "Verify", "detail": "One independent verifier per distinct (file, line) location \u2014 CONFIRMED / PLAUSIBLE / REFUTED per candidate"}, {"title": "Sweep", "detail": "Fresh finder hunting only for gaps (xhigh/max)"}, {"title": "Synthesize", "detail": "Merge duplicates, rank, cap the report"}],
}

// code-review: Scope \u2192 Find (barrier) \u2192 group-by-location \u2192 Verify \u2192 Sweep (xhigh/max) \u2192 Synthesize
// Effort parameterization mirrors the inline /code-review cells. Correctness
// keeps one finder per angle; cleanup is one finder covering all cleanup
// angles, capped at (cleanup-angle count \xD7 perAngle) so the merged finder
// retains the same total cleanup-candidate allowance as the old per-angle finders.
//   high  \u2192 3 correctness + 1 cleanup (5 angles, \u226430 cands) \u2192 \u226410 findings
//   xhigh \u2192 5 correctness + 1 cleanup (5 angles, \u226440 cands) \u2192 sweep \u2192 \u226415 findings
//   max   \u2192 same structure as xhigh (the API reasoning effort differs, not the fan-out)
const LEVEL_PARAMS = {
  high: { correctnessAngles: 3, perAngle: 6, maxFindings: 10, sweep: false, effort: "high" },
  xhigh: { correctnessAngles: 5, perAngle: 8, maxFindings: 15, sweep: true, effort: "xhigh" },
  max: { correctnessAngles: 5, perAngle: 8, maxFindings: 15, sweep: true, effort: "max" },
}
const SWEEP_MAX = 8

const RAW_ARGS = (typeof args === "string" ? args : "").trim()
const FIRST = RAW_ARGS.split(/\s+/)[0] || ""
// Own-property check so Object.prototype keys ("constructor", "toString") never parse as a level.
const FIRST_IS_LEVEL = Object.prototype.hasOwnProperty.call(LEVEL_PARAMS, FIRST)
const LEVEL = FIRST_IS_LEVEL ? FIRST : "high"
const TARGET = FIRST_IS_LEVEL ? RAW_ARGS.slice(FIRST.length).trim() : RAW_ARGS
const P = LEVEL_PARAMS[LEVEL]

// Prompt fragments shared with the inline /code-review cells (one source of truth).
const CORRECTNESS_ANGLES = [{"label": "angle-A", "text": "### Angle A \u2014 line-by-line diff scan\n\nRead every hunk in the diff, line by line. Then read the enclosing function for\neach hunk \u2014 bugs in unchanged lines of a touched function are in scope (the PR\nre-exposes or fails to fix them). For every line ask: what input, state, timing,\nor platform makes this line wrong? Look for inverted/wrong conditions,\noff-by-one, null/undefined deref, missing `await`, falsy-zero checks,\nwrong-variable copy-paste, error swallowed in catch, unescaped regex metachars.\n"}, {"label": "angle-B", "text": "### Angle B \u2014 removed-behavior auditor\n\nFor every line the diff DELETES or replaces, name the invariant or behavior it\nenforced, then search the new code for where that invariant is re-established.\nIf you can't find it, that's a candidate: a removed guard, a dropped error\npath, a narrowed validation, a deleted test that was covering a real case.\n"}, {"label": "angle-C", "text": "### Angle C \u2014 cross-file tracer\n\nFor each function the diff changes, find its callers (search for the symbol) and\ncheck whether the change breaks any call site: a new precondition, a changed\nreturn shape, a new exception, a timing/ordering dependency. Also check callees:\ndoes a parallel change in the same PR make a call unsafe?\n"}, {"label": "angle-D", "text": "### Angle D \u2014 language-pitfall specialist\n\nScan for the classic pitfalls of the diff's language/framework \u2014 for example:\nJS falsy-zero, `==` coercion, closure-captured loop var; Python mutable default\nargs, late-binding closures; Go nil-map write, range-var capture; SQL injection;\ntimezone/DST drift; float equality. Flag any instance the diff introduces.\n"}, {"label": "angle-E", "text": "### Angle E \u2014 wrapper/proxy correctness\n\nWhen the PR adds or modifies a type that wraps another (cache, proxy, decorator,\nadapter): check that every method routes to the wrapped instance and not back\nthrough a registry/session/global \u2014 e.g. a caching provider holding a\n`delegate` field that resolves IDs via `session.get(...)` instead of\n`delegate.get(...)` will re-enter the cache or recurse. Also check that the\nwrapper forwards all the methods the callers actually use.\n"}]
const CLEANUP_TEXT = "### Reuse\n\nFlag new code that re-implements something the codebase\nalready has \u2014 Search shared/utility modules and files adjacent to the change,\nand name the existing helper to call instead.\n\n\n### Simplification\n\nFlag unnecessary complexity the diff adds: redundant or derivable state,\ncopy-paste with slight variation, deep nesting, dead code left behind. Name\nthe simpler form that does the same job.\n\n\n### Efficiency\n\nFlag wasted work the diff introduces: redundant computation or repeated I/O,\nindependent operations run sequentially, blocking work added to startup or\nhot paths. Also flag long-lived objects built from closures or captured\nenvironments \u2014 they keep the entire enclosing scope alive for the object's\nlifetime (a memory leak when that scope holds large values); prefer a\nclass/struct that copies only the fields it needs. Name the cheaper\nalternative.\n\n\n### Altitude\n\nCheck that each change is implemented at the right depth, not as a fragile\nbandaid. Special cases layered on shared infrastructure are a sign the fix\nisn't deep enough \u2014 prefer generalizing the underlying mechanism over adding\nspecial cases.\n\n\n### Conventions (AGENTS.md)\n\nFind the AGENTS.md files that govern the changed code: the user-level\n~/.codex/AGENTS.md, the repo-root AGENTS.md, plus any AGENTS.md or\nAGENTS.override.md in a directory that is an ancestor of a changed file (a\ndirectory's AGENTS.md only applies to files at or below it). Read each one\nthat exists, then check the diff for clear violations of the rules they state.\n\nOnly flag a violation when you can quote the exact rule and the exact line\nthat breaks it \u2014 no style preferences, no vague \"spirit of the doc\"\ninferences. In the finding, name the AGENTS.md path and quote the rule so the\nreport can cite it. If no AGENTS.md applies, return nothing for this angle.\n"
const VERDICT_LADDER = "- **CONFIRMED** \u2014 can name the inputs/state that trigger it and the wrong\n  output or crash. Quote the line.\n- **PLAUSIBLE** \u2014 mechanism is real, trigger is uncertain (timing, env,\n  config). State what would confirm it.\n- **REFUTED** \u2014 factually wrong (code doesn't say that) or guarded elsewhere.\n  Quote the line that proves it."
const VERDICT_LADDER_RECALL = "**PLAUSIBLE by default** \u2014 do not refute a candidate for being \"speculative\" or\n\"depends on runtime state\" when the state is realistic: concurrency races,\nnil/undefined on a rare-but-reachable path (error handler, cold cache, missing\noptional field), falsy-zero treated as missing, off-by-one on a boundary the\ncode does not exclude, retry storms / partial failures, regex/allowlist that\nlost an anchor. These are PLAUSIBLE.\n\n**REFUTED** only when constructible from the code: factually wrong (quote the\nactual line); provably impossible (type/constant/invariant \u2014 show it); already\nhandled in this diff (cite the guard); or pure style with no observable effect."

// \u2500\u2500\u2500 Schemas \u2500\u2500\u2500
const SCOPE_SCHEMA = {
  type: "object", required: ["diffCommand", "files", "summary"],
  properties: {
    diffCommand: { type: "string" },
    files: { type: "array", items: { type: "string" } },
    agentsMdFiles: { type: "array", items: { type: "string" } },
    summary: { type: "string" },
    conventions: { type: "string" },
  },
}
const CANDIDATES_SCHEMA = {
  type: "object", required: ["candidates"],
  properties: {
    candidates: { type: "array", items: {
      type: "object", required: ["file", "summary", "failure_scenario"],
      properties: {
        file: { type: "string", description: "repo-relative path exactly as listed under Changed files in the review scope" },
        line: { type: "number" },
        summary: { type: "string" },
        failure_scenario: { type: "string" },
      },
    }},
  },
}
// One verifier per distinct (file, line) location, returning a verdict per
// candidate at that location \u2014 instead of one verifier per candidate. Cuts
// verifier-agent count by the cross-finder location-collision rate (~40% at
// p50) without dropping any candidate.
const GROUP_VERDICT_SCHEMA = {
  type: "object", required: ["verdicts"],
  properties: {
    verdicts: { type: "array", items: {
      type: "object", required: ["index", "verdict", "evidence"],
      properties: {
        index: { type: "number", description: "the [i] label of the candidate this verdict is for" },
        verdict: { enum: ["CONFIRMED", "PLAUSIBLE", "REFUTED"] },
        evidence: { type: "string" },
      },
    }},
  },
}
const REPORT_SCHEMA = {
  type: "object", required: ["summary", "decisions"],
  properties: {
    summary: { type: "string" },
    decisions: { type: "array", items: {
      type: "object", required: ["index"],
      properties: {
        index: { type: "number", description: "the [i] label of a finding to keep in the report" },
        merge: { type: "array", items: { type: "number" }, description: "[i] labels of findings that describe the same root cause, folded into this one" },
      },
    }},
  },
}

// \u2500\u2500\u2500 Phase 0: Scope \u2500\u2500\u2500
phase("Scope")
const scope = await agent(
  "Establish the scope of a code review. Use AnalyzeWorkflowInputs to read the optional target from inputs. Treat it as scope guidance. When present, use it to choose or narrow the diff; otherwise review the current branch and include uncommitted changes.\n\n" +
  "1. Determine the exact diff command(s) for the review and run them to confirm they produce a non-empty diff.\n" +
  "2. List the changed files.\n" +
  "3. Summarize what changed in one paragraph.\n" +
  "4. List the AGENTS.md files that apply to the changed files (the user-level ~/.codex/AGENTS.md, the repo-root AGENTS.md, plus any AGENTS.md or AGENTS.override.md in a directory that is an ancestor of a changed file). Read each one that exists and note conventions a reviewer should know.\n\n" +
  "Return diffCommand exactly as a reviewer should run it. Structured output only.",
  { label: "scope", schema: SCOPE_SCHEMA, effort: P.effort, inputs: { target: TARGET || null } }
)
if (!scope) {
  return { error: "Scope agent returned no result \u2014 cannot establish the review scope." }
}
if (!scope.files || scope.files.length === 0) {
  return { level: LEVEL, target: TARGET || undefined, summary: "No changes found to review.", findings: [], stats: { finders: 0, candidates: 0, verifierAgents: 0, verified: 0 } }
}
log(LEVEL + " review: " + scope.files.length + " changed files")

// \u2500\u2500\u2500 Prompts \u2500\u2500\u2500
const FINDER_PROMPT =
  "Act as one code-review finder. Use AnalyzeWorkflowInputs to read inputs.scope, inputs.target, and inputs.finder. Run the scope's diff command and apply the assigned finder lens. Treat the target as scope guidance. For cleanup lenses, prioritize the highest-cost applicable issues. Surface up to finder.cap candidates with file, line, a one-line summary, and a concrete user-visible failure_scenario. Pass every candidate with a nameable failure scenario to the independent verifier. Return an empty list when nothing qualifies. Structured output only."

// Finders may return absolute, repo-relative, or backslash-separated paths
// for the same file. Normalize once at ingest by suffix-matching against
// scope.files (which the Scope agent returns repo-relative) so every
// downstream consumer \u2014 group key, verifier inputs, synthesis inputs,
// final report \u2014 sees the same path. Longest match wins so that when one
// changed-file path is itself a suffix of another (util/x.ts vs a/util/x.ts),
// an absolute path canonicalizes to the more-specific entry.
const canonFile = raw => {
  if (!raw) return ""
  const p = raw.replace(/\\/g, "/")
  let best = ""
  for (const sf of scope.files) {
    if ((p === sf || p.endsWith("/" + sf)) && sf.length > best.length) best = sf
  }
  return best || p
}
const ingest = (cs, cap, kind) => cs.slice(0, cap).map(c => ({ ...c, file: canonFile(c.file), kind }))
const loc = c => c.file + (c.line != null ? ":" + c.line : "")
const inBounds = (i, n) => Number.isInteger(i) && i >= 0 && i < n

const GROUP_VERIFIER_PROMPT =
  "Act as a code-review verifier. Use AnalyzeWorkflowInputs to read inputs.scope, inputs.target, and every indexed candidate in inputs.group. Run the scope's diff command, read the relevant files, and return one verdict per candidate. " +
  "Judge EACH candidate independently on its own claim \u2014 candidates at the same location may describe distinct issues, the same issue, or a mix. " +
  "Reference each by its [i] index.\n\n" +
  VERDICT_LADDER + "\n\n" + VERDICT_LADDER_RECALL + "\n\n" +
  "Structured output only. Evidence must quote or cite the relevant line(s)."

// \u2500\u2500\u2500 Same-location verifier merge \u2014 group ingested candidates by loc(c),
// one verifier agent per location returning N verdicts. Grouping is not
// dedup: every candidate keeps its own verdict; the synthesis step merges
// semantic dupes. A candidate the verifier did not render a verdict on
// (agent died, or it omitted that index) is dropped \u2014 same policy as the
// old per-candidate verifier \u2014 so unverified candidates never reach the
// report as fabricated PLAUSIBLE. Trade-off vs per-candidate: one verifier-
// agent failure now drops every candidate at that location instead of one.
let verifierAgents = 0

async function verifyGroups(candidates) {
  const byLoc = Object.create(null)
  for (const c of candidates) (byLoc[loc(c)] ||= []).push(c)
  const groups = Object.values(byLoc)
  verifierAgents += groups.length
  const out = await parallel(groups.map(g => async () => {
    const short = g[0].file.split("/").pop()
    const r = await agent(GROUP_VERIFIER_PROMPT, {
      label: "verify:" + short + ":" + (g[0].line == null ? "file" : g[0].line) + "(" + g.length + ")",
      phase: "Verify",
      schema: GROUP_VERDICT_SCHEMA,
      effort: P.effort,
      inputs: { scope, target: TARGET || null, group: g },
    })
    if (!r) return []
    const byIdx = {}
    for (const v of r.verdicts) if (inBounds(v.index, g.length)) byIdx[v.index] = v
    return g.flatMap((c, i) => byIdx[i] ? [{ ...c, verdict: byIdx[i].verdict, evidence: byIdx[i].evidence }] : [])
  }))
  return out.filter(Boolean).flat()
}

// \u2500\u2500\u2500 Find (barrier) \u2192 group \u2192 Verify. The barrier is the deliberate trade
// for cross-finder location merge: grouping needs every finder's output.
// Correctness stays 1 finder per angle (lens-partitioning matters for catch).
// Cleanup is ONE finder covering all cleanup angles (same shared texts, one
// agent) \u2014 keeps the task set identical to inline, breaks only the
// 1-angle:1-agent mapping. With four fewer finders at every level the
// barrier wait shortens enough that wall-clock is net-faster than the
// pre-#45024 per-finder pipeline.
const FINDERS = CORRECTNESS_ANGLES.slice(0, P.correctnessAngles)
  .map(a => ({ ...a, kind: "correctness", cap: P.perAngle }))
  .concat([{
    label: "cleanup",
    kind: "cleanup",
    cap: 5 * P.perAngle,
    text: CLEANUP_TEXT,
  }])

const finderResults = await parallel(FINDERS.map(f => () =>
  agent(FINDER_PROMPT, {
    label: f.label,
    phase: "Find",
    schema: CANDIDATES_SCHEMA,
    effort: P.effort,
    inputs: { scope, target: TARGET || null, finder: f },
  })
), { requireAll: true })
const allCandidates = finderResults.flatMap((result, index) => {
  const finder = FINDERS[index]
  log(finder.label + ": " + result.candidates.length + " candidates")
  return ingest(result.candidates, finder.cap, finder.kind)
})
let candidatesSeen = allCandidates.length

let verified = await verifyGroups(allCandidates)

// \u2500\u2500\u2500 Sweep (xhigh/max): one fresh finder hunting only for gaps \u2500\u2500\u2500
if (P.sweep) {
  phase("Sweep")
  const sweep = await agent(
    "Act as the gap-finding sweep for a code review. Use AnalyzeWorkflowInputs to read inputs.scope, inputs.target, and all inputs.knownCandidates. Re-read the diff and enclosing functions for defects not represented in the known candidates. Focus on moved or extracted code that dropped guards, language footguns, setup/teardown asymmetry, and flipped config defaults. Surface up to inputs.maxCandidates additional candidates and return an empty list when nothing new qualifies. Structured output only.",
    {
      label: "sweep",
      phase: "Sweep",
      schema: CANDIDATES_SCHEMA,
      effort: P.effort,
      inputs: {
        scope,
        target: TARGET || null,
        knownCandidates: verified,
        maxCandidates: SWEEP_MAX,
      },
    }
  )
  if (sweep && sweep.candidates.length > 0) {
    const sliced = ingest(sweep.candidates, SWEEP_MAX, "correctness")
    candidatesSeen += sliced.length
    log("sweep: " + sliced.length + " candidates")
    const sweepVerified = await verifyGroups(sliced)
    verified = verified.concat(sweepVerified)
  }
}

const surviving = verified.filter(c => c.verdict !== "REFUTED")
const refuted = verified.filter(c => c.verdict === "REFUTED")
log("Verify done: " + verified.length + " verified \u2192 " + surviving.length + " kept, " + refuted.length + " refuted")

const stats = {
  level: LEVEL,
  finders: FINDERS.length,
  candidates: candidatesSeen,
  verifierAgents,
  verified: verified.length,
  refuted: refuted.length,
}

if (surviving.length === 0) {
  return {
    level: LEVEL, target: TARGET || undefined,
    summary: "No findings survived verification.",
    findings: [],
    stats,
  }
}

// \u2500\u2500\u2500 Synthesize: rank, merge semantic dupes, cap \u2500\u2500\u2500
phase("Synthesize")
// Correctness bugs outrank cleanup findings when the cap forces a cut;
// CONFIRMED outranks PLAUSIBLE within each group.
const rank = c => (c.kind === "cleanup" ? 2 : 0) + (c.verdict === "PLAUSIBLE" ? 1 : 0)
const ranked = surviving.slice().sort((a, b) => rank(a) - rank(b))

const [report] = await parallel([async () => {
  const result = await agent(
    "Synthesize the final code-review report. Use AnalyzeWorkflowInputs to inspect every indexed finding in inputs.ranked plus inputs.level and inputs.maxFindings.\n\n" +
    "Represent each finding in the decisions by its index.\n" +
    "1. For each distinct defect, emit one decision with its index. When several findings describe the same defect (same root cause), keep one entry and list the others in its merge array.\n" +
    "2. Order decisions most-severe first. Correctness bugs always outrank cleanup findings.\n" +
    "3. Use inputs.maxFindings as the report cap, selecting the most severe decisions.\n" +
    "4. Write a complete summary of the review.\n\nStructured output only.",
    {
      label: "synthesize",
      schema: REPORT_SCHEMA,
      effort: P.effort,
      inputs: { ranked, level: LEVEL, maxFindings: P.maxFindings },
    }
  )
  if (!result || typeof result !== "object" || typeof result.summary !== "string" || !Array.isArray(result.decisions)) {
    throw new Error("Final code-review synthesis must return a valid structured report.")
  }
  return result
}], { requireAll: true })

// Assembler invariants:
//   1. No silent drops while there is room: every verified finding either appears
//      (as primary or merge note) or is omitted only because the cap is full.
//   2. The displayed primary is the synthesizer's choice (d.index) \u2014 it picks the
//      best-described representative; we only escalate the verdict label when a
//      merged member is CONFIRMED.
//   3. The summary describes the report actually returned.
const decisions = report.decisions
const seen = new Set()
const claim = i => (inBounds(i, ranked.length) && !seen.has(i) ? (seen.add(i), true) : false)
const findings = []
for (const d of decisions) {
  if (findings.length >= P.maxFindings) break
  if (!claim(d.index)) continue
  const c = ranked[d.index]
  const merged = (Array.isArray(d.merge) ? d.merge : []).filter(claim).map(i => ranked[i])
  const verdict = merged.some(m => m.verdict === "CONFIRMED") ? "CONFIRMED" : c.verdict
  const also = merged.length > 0 ? " [same root cause also at: " + merged.map(loc).join(", ") + "]" : ""
  findings.push({ file: c.file, line: c.line, summary: c.summary + also, failure_scenario: c.failure_scenario, category: c.kind, verdict })
}
const usedDecisions = findings.length > 0
let backfilled = 0
for (let i = 0; i < ranked.length && findings.length < P.maxFindings; i++) {
  if (seen.has(i)) continue
  const c = ranked[i]
  findings.push({ file: c.file, line: c.line, summary: c.summary, failure_scenario: c.failure_scenario, category: c.kind, verdict: c.verdict })
  backfilled++
}
const summary = usedDecisions
  ? report.summary + (backfilled > 0 ? " (" + backfilled + " additional verified finding" + (backfilled === 1 ? "" : "s") + " appended unmerged.)" : "")
  : "Synthesis returned no usable decisions \u2014 returning verified findings ranked, unmerged."

return {
  level: LEVEL,
  target: TARGET || undefined,
  summary,
  findings,
  refuted: refuted.map(c => ({ file: c.file, line: c.line, summary: c.summary })),
  stats: { ...stats, reported: findings.length },
}
