---
name: implement-task
description: Run one Controlz task (T# or B#) end to end - optional design spec, implementer subagent, reviewer subagent, up to two fix rounds, then update plan.md and commit. Use when the user names a task to implement or after /next picks one.
argument-hint: "<task-id>"
---

# /implement-task $ARGUMENTS

You are the coordinator. You never write product code here; you brief subagents, judge their reports and keep `docs/plan.md` true.

## 1. Prepare
- Read the task block for `$ARGUMENTS` in `docs/plan.md`. Stop if it doesn't exist, is `done` (unless the user asked to redo it), or has deps that aren't `done`.
- Read the knowledge-base headings listed in its `kb` field, plus **Conventions**, **Gotchas** and the active **Decisions**.
- Check `git status`. If there are uncommitted changes that don't belong to this task, ask the user before continuing.
- Set `status: in_progress` in the plan.

## 2. Design (only when `owner: designer`)
Dispatch `designer` in spec mode with the task block, the relevant knowledge-base excerpts and the `macos-design` template. Show the user the spec summary and wireframe and ask for approval. On approval, set the spec's `status: approved`, mark the task `done` and go to step 6. Implementation tasks never start from an unapproved spec.

## 3. Implement
Dispatch `implementer` with a self-contained brief:
```
Task: <full task block, verbatim>
Read first: docs/knowledge_base.md → <headings>; <design spec path, if any>
Context: <anything the plan notes or the user said that matters>
Constraints: follow task-contract; do not commit; do not edit docs/plan.md
Return: the task-contract report
```
- `status: blocked` → set the task `blocked`, put the question in `notes`, ask the user, and stop.
- `partial` → decide with the user: continue in a new round, or split it with `plan-adjust`.

## 4. Review
Set `status: in_review`. Dispatch `reviewer` with the task block, the implementer's report and the changed-file list. Don't add your own opinion of the code to the brief.
- **PASS** → step 5.
- **FAIL** → send the implementer a fix brief: task block + the reviewer's findings verbatim + "fix only these". Then review again. **At most 2 fix rounds.** After a second FAIL, set the task `blocked` with the outstanding findings in `notes` and ask the user how to proceed.
- If the reviewer reports `kb_conflicts`, resolve them with `kb-update` before marking the task done.

**UI tasks:** if the user wants design review, ask them for a screenshot of the running app (or the implementer's `screencapture` output), then dispatch `designer` in review mode. Treat its FAIL like a reviewer FAIL.

## 5. Close
- Tick the acceptance boxes that were verified. Set `status: done`. Put follow-ups in `notes`. Propose new tasks or bugs from `follow_ups` via `plan-adjust`, but don't add them silently.
- Make sure any `kb_additions` from the implementer are in place and well-placed.
- Add a **Session log** line: `<YYYY-MM-DD> · <ID> · done · <one-line summary> · <short commit hash>`.

## 6. Commit (if the commit policy decision in the knowledge base says yes)
Stage only this task's files plus `docs/`:
```
git add <files_changed from the report> docs/
git commit -m "<ID>: <task title>" -m "<summary from the report>"
```
Then put the short hash into the session log line.

## 7. Report to the user
3–6 lines: what was done, how it was verified, anything they must do by hand (e.g. accept the pairing prompt on the TV), and the next task.
