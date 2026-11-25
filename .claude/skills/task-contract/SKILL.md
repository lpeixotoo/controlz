---
name: task-contract
description: The contract between the coordinator and its subagents (implementer, reviewer, designer) in Controlz - task brief anatomy, scope rules, task statuses and the report format every subagent must return.
user-invocable: false
---

# Task contract

## Where truth lives
- `docs/plan.md`: tasks and bugs. Only the coordinator edits it.
- `docs/knowledge_base.md`: facts, decisions (`D#`), conventions, gotchas. The implementer may **append** facts with a source; it never rewrites or deletes entries.
- `docs/design/<screen>.md`: UI specs, written by the designer only.

If the brief, the knowledge base and the code disagree, stop and report the conflict. Don't silently pick one.

## Task block (as it appears in plan.md)
```markdown
### T1.2 — webOS connect + pairing
- status: todo
- deps: T0.2, T1.1
- owner: implementer            # implementer | designer
- files: core/src/webos.rs      # expected footprint; touching more is allowed only if necessary, and must be reported
- kb: TV / webOS protocol       # knowledge_base.md headings to read
- design: -                     # docs/design/*.md for UI tasks
- acceptance:
  - [ ] concrete, checkable criterion
- notes: free text, links to B#/D#
```
Statuses: `todo` → `in_progress` → `in_review` → `done`; or `blocked` (with a reason in notes) and `reopened` (with a link to the bug or correction).

## Scope rules
- Do exactly the task. No drive-by refactors, renames or dependency upgrades outside `files`.
- If something outside scope is broken, mention it under `follow_ups`; don't fix it.
- New dependencies: allowed only when the task needs them; justify each one in the report.
- Never commit, push, or change git state. The coordinator commits.
- Never put secrets (ThinQ token, TV client-key) in code, tests, fixtures or docs. Use obviously fake values in fixtures.

## Report format (every subagent ends with this, nothing after it)
```
task: T1.2
status: done | blocked | partial
summary: 1–3 sentences on what changed and why
files_changed:
  - path — what changed
verification:
  - command → result (paste the real final lines)
acceptance:
  - [x] criterion — evidence (test name, file:line, screenshot)
  - [ ] criterion — why not
kb_additions: headings/lines appended to knowledge_base.md, or "none"
decisions_needed: questions only the user/coordinator can answer, or "none"
follow_ups: out-of-scope issues noticed, or "none"
```
The reviewer and designer use the same frame, with `verdict: PASS | FAIL` and `findings` as defined in their agent instructions.
