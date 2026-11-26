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
- Don't change the status yet; it moves to `in_progress` once the user approves the brief.

## 2. Design (only when `owner: designer`)
First dispatch `designer` with `mode: brief` and show the user its brief (D16); continue only on approval. Then dispatch `designer` in spec mode (`mode: implement`) with the task block, the relevant knowledge-base excerpts and the `macos-design` template. Show the user the spec summary and wireframe and ask for approval. On approval, set the spec's `status: approved`, mark the task `done` and go to step 6. Implementation tasks never start from an unapproved spec.

## 3. Implement
### 3a. Brief (D16)
Dispatch `implementer` with the brief below and `Mode: brief`. Show the user its returned brief as a short summary: plan, files, **every outside-the-repo change (exact command/line)**, new dependencies, side effects, questions. Ask for approval. Only then set `status: in_progress` and continue. Changes outside the repo that the user didn't approve never happen (D15).

### 3b. Implement
Dispatch `implementer` again with `Mode: implement` and the approved brief pasted in:
```
Mode: brief | implement
Approved brief: <the brief the user approved, in implement mode>
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
- **FAIL** → tell the user the findings and the intended fixes; if a fix needs anything outside the approved brief (new files, deps, outside-the-repo changes), get approval first. Then send the implementer a fix brief: task block + approved brief + the reviewer's findings verbatim + "fix only these". Then review again. **At most 2 fix rounds.** After a second FAIL, set the task `blocked` with the outstanding findings in `notes` and ask the user how to proceed.
- If the reviewer reports `kb_conflicts`, resolve them with `kb-update` before marking the task done.

**UI tasks:** if the user wants design review, ask them for a screenshot of the running app (or the implementer's `screencapture` output), then dispatch `designer` in review mode. Treat its FAIL like a reviewer FAIL.

## 5. Overview, then wait for the user (D14)
After a reviewer PASS, **stop**. Leave the task at `in_review`: don't tick boxes, don't set `done`, don't commit.
- Make sure any `kb_additions` from the implementer are in place and well-placed; resolve `kb_conflicts`.
- Put the review result and follow-ups in the task's `notes`. Propose new tasks or bugs from `follow_ups` via `plan-adjust` only if the user agrees.
- Give the user an overview: what changed (files), how it was verified, the review verdict and findings, follow-ups, and anything they must do by hand. Ask whether to close and commit.

## 6. Close and commit (only when the user says so)
- Tick the verified acceptance boxes, set `status: done`, add a **Session log** line: `<YYYY-MM-DD> · <ID> · done · <one-line summary> · <short commit hash>`.
- If the user asked to commit, stage only this task's files plus `docs/`:
```
git add <files_changed from the report> docs/
git commit -m "<ID>: <task title>" -m "<summary from the report>"
```
Then put the short hash into the session log line.

## 7. Report to the user
After closing: 3–6 lines: what was done, how it was verified, anything they must do by hand (e.g. accept the pairing prompt on the TV), and the next task.
