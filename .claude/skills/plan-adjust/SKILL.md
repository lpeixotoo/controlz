---
name: plan-adjust
description: Change docs/plan.md for Controlz - add, split, merge, reorder, re-scope, block or drop tasks and phases, keeping dependencies valid. Use when the user wants to tweak the plan or when a correction or bug implies plan changes.
argument-hint: "<what to change>"
---

# /plan-adjust

Request: $ARGUMENTS

## Current plan
```!
scripts/plan-status.sh
```

## Do
1. Read `docs/plan.md` in full and the relevant parts of `docs/knowledge_base.md`. Restate the change in one sentence.
2. Draft the edit following the task format in `.claude/skills/bootstrap/templates/task.md`:
   - **Add:** next free ID in that phase; deps, files, kb, 2–5 acceptance criteria.
   - **Split:** keep the original ID for the first part, add new IDs for the rest; fix anything that depended on it.
   - **Reorder:** change `deps`, not just position. Position is for readability; `deps` decide execution order.
   - **Drop:** confirm with the user first; set `status: blocked` with `notes: dropped — <reason>` unless they want it deleted.
   - **Re-scope a done task:** that's a reopen. Confirm with the user and set `status: reopened` with the reason.
3. Validate:
   - every dep ID exists, no cycles, no task depends on a dropped task
   - UI tasks still depend on their design task
   - `scripts/plan-status.sh` parses (run it after editing)
4. Ask before applying if the change touches `done` work, a decision `D#`, or more than one phase. Otherwise apply directly.
5. No Change log line (D18: the log records only finished tasks).
6. Reply with the diff summary (IDs added/changed/removed) and the new next task.
