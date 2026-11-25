---
name: review
description: Run the read-only reviewer subagent on demand - against a task ID, the uncommitted diff, or a commit range - and report the verdict. Use when the user asks for a review outside the normal implement-task loop.
argument-hint: "[task-id | diff | <commit-range>]"
---

# /review $ARGUMENTS

1. Work out the target:
   - a task ID → the task block from `docs/plan.md` plus its commit(s) (find them with `git log --grep "<ID>:"`)
   - `diff` or empty → the uncommitted changes (`git status`, `git diff`)
   - a commit range → that range
2. Dispatch `reviewer` with: the target, the task block(s) and acceptance criteria if any, and the knowledge-base headings that apply.
3. Relay the verdict and findings to the user unchanged (you can shorten wording, but keep severities and file:line).
4. Don't change task statuses yourself. If findings show a `done` task is wrong, offer to turn them into a bug with `report-bug`.
