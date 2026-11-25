---
name: bootstrap
description: Create (or rebuild) docs/plan.md and docs/knowledge_base.md for Controlz from an interview with the user plus research. Run once at the start, or to start the plan over.
disable-model-invocation: true
argument-hint: "[--rebuild]"
---

# /bootstrap

Goal: leave the repo with a complete `docs/knowledge_base.md` and `docs/plan.md` that any fresh session can execute task by task.

## Current state
```!
ls docs/ 2>/dev/null; echo "---"; scripts/plan-status.sh --short; echo "---"; git log --oneline -5 2>/dev/null || echo "no commits"; echo "---"; ls core app 2>/dev/null || true
```

## 0. Guard
- If `docs/plan.md` exists and `$ARGUMENTS` is not `--rebuild`, stop. Tell the user to use `plan-adjust` / `kb-update`, or re-run `/bootstrap --rebuild`.
- On `--rebuild`, keep every `done` task and every decision `D#` unless the user explicitly drops them.
- `docs/knowledge_base.md` may already contain seeded research. **Merge into it, never discard it.**

## 1. Interview (AskUserQuestion, at most 4 questions per round)
Ask only what the knowledge base doesn't already answer. Required:
1. TV: model / year (webOS version if known), IP address, MAC address (for Wake-on-LAN); whether "Turn on via Wi-Fi" is enabled.
2. AC: model, whether it appears in the LG ThinQ app; country of the LG account (selects the ThinQ region).
3. Workflow: one git commit per finished task? (recommended: yes). Design review by screenshots: yes/no?
4. Existing code in `core/` (if any): keep it as a starting point to review, or delete it?
5. Minimum macOS version (recommended: the current one on this Mac, check `sw_vers`).

Unknowns are fine. Record them under **Open questions** with the task that needs them.

## 2. Research gaps
Fill knowledge-base gaps that block planning (e.g. a specific AC model's ThinQ profile shape, a webOS version quirk). Cite every source. Don't spawn agents for this.

## 3. Write the knowledge base
Use [templates/knowledge_base.md](templates/knowledge_base.md). Record the interview answers as facts or decisions (`D#`, with date and rationale).

## 4. Write the plan
Use [templates/plan.md](templates/plan.md) and the task format in [templates/task.md](templates/task.md). Rules:
- Phases follow the **Roadmap sketch** in the knowledge base unless the user changes it.
- Tasks are small: one focused change, reviewable in one sitting, ideally ≤ ~300 changed lines, with 2–5 checkable acceptance criteria.
- Every task names its `files`, `kb` headings and `deps`. No dependency cycles.
- Put a CLI or test harness early, so the device protocols can be verified against real hardware before any UI exists.
- UI implementation tasks depend on a designer task that produces the spec.
- Tasks that need the user (pairing the TV, creating a ThinQ token) say so in `notes`.

## 5. Review with the user
Show the phase list and the first ~5 tasks in a compact table. Apply their changes, then write the files.

## 6. Finish
- Add a change-log entry: `<date> — bootstrap — initial plan`.
- Run `scripts/plan-status.sh` and show the result.
- If the user agreed to commits: `git add docs/ CLAUDE.md README.md .claude/ scripts/ && git commit -m "docs: bootstrap plan and knowledge base"`.
- End with the next task and tell them to say "next".
