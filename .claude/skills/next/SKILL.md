---
name: next
description: Pick the next unblocked task in docs/plan.md (critical bugs and reopened tasks first) and run it through implement-task. Use when the user says next, continue, or keep going.
---

# /next

## Plan state
```!
scripts/plan-status.sh
```

## Do
1. If a task is already `in_progress` or `in_review`, resolve that first (resume it, or reset it to `todo` after checking `git status` with the user). Don't start a second task.
2. Choose the task, in this order:
   1. `critical` bugs whose deps are done
   2. the `next:` task reported by `plan-status.sh` (reopened first, then the first `todo` with all deps done)
3. If the chosen task needs something only the user can provide (see its `notes` and **Open questions**), ask for it now. If they can't provide it, pick the next one that doesn't need it and tell them.
4. Say in one line which task you're starting and why, then follow the `implement-task` skill for that ID.
5. After it finishes, report the result and the following task, then stop. Run one task per `/next` unless the user asked for several ("do the next 3").
