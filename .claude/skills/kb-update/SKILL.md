---
name: kb-update
description: Record a fact, correction or decision in docs/knowledge_base.md for Controlz and work out which tasks it affects. Use when the user shares device details, corrects an assumption, makes a technical decision, or a subagent reports a knowledge-base conflict.
argument-hint: "<fact, correction or decision>"
---

# /kb-update

Input: $ARGUMENTS

## Do
1. Read `docs/knowledge_base.md` in full. Classify the input:
   - **Fact** (device model, IP, API behavior): add it under the right heading with its source (`source: user, <date>` or a URL).
   - **Correction** (contradicts an existing entry): replace the entry and leave a short `(corrected <date>: was X)` note so history stays readable.
   - **Decision**: add `D<next>` under **Decisions** as `date · decision · rationale · status: active`. If it replaces an older one, mark that one `superseded by D<n>`. Ask the user before superseding.
   - **Answer to an open question**: record the answer, then remove the question from both the knowledge base and the plan's **Open questions**.
2. **Impact analysis:** search `docs/plan.md` for tasks whose `kb`, `files`, acceptance or notes relate to what changed.
   - `todo` tasks: update their acceptance or notes directly if the change is mechanical.
   - `done` tasks whose behavior is now wrong: list them and ask the user whether to reopen them. If yes, hand off to `plan-adjust`.
   - New work implied: propose tasks and hand off to `plan-adjust`.
3. Add a **Change log** line in `docs/plan.md`: `<YYYY-MM-DD> · kb: <what> · <why>`.
4. Reply: what was recorded, which tasks are affected, what (if anything) you need the user to confirm.
