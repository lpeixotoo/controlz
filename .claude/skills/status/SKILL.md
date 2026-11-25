---
name: status
description: Report where the Controlz project stands - task counts, what's in flight or blocked, the next unblocked task, recent commits and open questions. Read-only. Use when the user asks for status, progress or what's next.
---

# /status

## Live data
```!
scripts/plan-status.sh
echo "--- git"
git log --oneline -8 2>/dev/null || echo "no commits yet"
git status --short 2>/dev/null | head -20
```

## Do
1. If there is no plan, say so and point to `/bootstrap`.
2. Read `docs/plan.md` → **Open questions**, the last ~5 **Session log** lines, and any `blocked`/`reopened` task blocks.
3. Reply in at most ~12 lines:
   - one-line progress summary (per phase if helpful)
   - in flight / blocked / reopened, each with its reason
   - uncommitted changes, if `git status` shows any, and whether they belong to an in-flight task
   - **next:** the next task ID + title + one line on what it does
   - open questions that block upcoming tasks
4. Don't change any file.
