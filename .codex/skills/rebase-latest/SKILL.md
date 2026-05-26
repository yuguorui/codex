---
name: rebase-latest
description: Rebase the current Codex branch on the latest upstream origin/main, preserve local/user changes, resolve conflicts safely, validate the build/tests, and push the rebased branch to the fork when requested. Use when the user asks to rebase, rebase origin, update from upstream, sync with latest origin/main, or prepare a branch after rebase.
---

# Rebase Latest

## Goal

Rebase the current branch on the latest upstream `origin/main` without losing work, validate the result, and push to the fork only when requested.

This repo normally uses:

- `origin`: upstream `https://github.com/openai/codex.git`
- `my`: user fork `git@github.com:yuguorui/codex.git`
- working branch: usually `main`

Confirm these from local git state instead of assuming.

## Safety Rules

- Start with `git status --short --branch`.
- If there are uncommitted changes, inspect them. Do not overwrite or discard user changes.
- Do not use `git reset --hard`, `git checkout --`, or other destructive commands unless the user explicitly asks.
- Prefer non-interactive commands.
- If rebasing published fork history, push with `--force-with-lease`, not plain force.
- If conflicts involve files changed by the user in the working tree before the task, preserve user intent and ask only if the conflict is ambiguous.
- If the user asked to push/build after rebase, complete those steps in the same turn when feasible.

## Standard Workflow

1. Inspect state:

   ```bash
   git status --short --branch
   git remote -v
   git log --oneline --decorate -8
   ```

2. Fetch upstream:

   ```bash
   git fetch origin
   ```

3. Rebase onto upstream main:

   ```bash
   git rebase origin/main
   ```

   If an interactive autosquash rebase is explicitly needed, use:

   ```bash
   GIT_SEQUENCE_EDITOR=: git rebase -i --autosquash origin/main
   ```

4. Resolve conflicts conservatively:

   - Use `git status --short` to list conflicted files.
   - Read both sides before editing.
   - Keep unrelated user edits.
   - After editing a conflicted file, run `git add <file>`.
   - Continue with `git rebase --continue`.
   - Repeat until complete.

5. Run validation appropriate to the touched area.

   For Rust changes under `codex-rs`:

   ```bash
   just fmt
   just test -p <changed-crate>
   just fix -p <changed-crate>
   ```

   Use `codex-core` for core changes. Do not run `cargo test` directly.

   For workflow-only or docs-only changes, use the narrowest relevant validation. If no local build applies, state that explicitly.

6. Check final state:

   ```bash
   git status --short --branch
   git log --oneline --decorate -5
   ```

7. If the user asked to push to the fork:

   ```bash
   git push --force-with-lease my <branch>
   ```

8. If the user asked to trigger the fork release build:

   ```bash
   gh workflow run fork-release.yml -R yuguorui/codex --ref <branch>
   gh run view -R yuguorui/codex <run-id> --json name,status,conclusion,event,headBranch,headSha,url,createdAt
   ```

## Remote Scheduled Workflow

For remote periodic rebase, prefer the GitHub Actions workflow at
`.github/workflows/fork-auto-rebase.yml`.

Use a two-stage design:

1. Plain Git path first:

   ```bash
   git fetch upstream main
   git rebase upstream/main
   ```

2. If the plain rebase fails, invoke `openai/codex-action` as the conflict resolver.

   Required action settings:

   - `safety-strategy: drop-sudo`
   - `sandbox: workspace-write`
   - `openai-api-key: ${{ secrets.CODEX_OPENAI_API_KEY }}`
   - prompt must explicitly say not to push

3. After Codex returns, the workflow must verify:

   ```bash
   test ! -d .git/rebase-merge
   test ! -d .git/rebase-apply
   git status --porcelain=v1 -uall
   ```

   The status output must be empty before pushing.

4. Push only from a deterministic shell step:

   ```bash
   git push --force-with-lease origin HEAD:main
   ```

Do not let Codex push directly from the action prompt. Keeping push in a
separate workflow step makes the side effect visible and keeps retry behavior
predictable.

If Codex cannot resolve the rebase safely, the workflow should fail and require
manual intervention.

## Reporting

In the final response, include:

- final branch and head SHA
- whether rebase completed cleanly or conflicts were resolved
- validation commands run and results
- whether push happened, including remote/branch
- workflow URL if a fork build was triggered
