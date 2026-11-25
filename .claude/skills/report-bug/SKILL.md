---
name: report-bug
description: Turn a bug report about Controlz into a tracked B# task in docs/plan.md - gather repro details, optionally have the reviewer investigate the root cause, set severity and reopen affected tasks. Use when the user says something is broken or behaves wrong.
argument-hint: "<what went wrong>"
---

# /report-bug

Report: $ARGUMENTS

## Do
1. **Clarify** (AskUserQuestion, only what's missing): steps to reproduce, expected vs actual, which device and how often, any error text shown. Don't interrogate; two rounds at most.
2. **Locate:** use `docs/plan.md` and `git log` to find which `done` task(s) own the behavior.
3. **Investigate (optional, recommended for anything not obvious):** dispatch the `reviewer` subagent in *investigate mode* with the report, the suspected task blocks and the relevant knowledge-base headings. Use its root cause, file:line evidence and proposed fix in the bug task.
4. **Severity:** `critical` (app crashes, data or secret leak, device stuck), `major` (a core action fails), `minor` (cosmetic, workaround exists).
5. **Write the bug** under `## Bugs` using the bug format in `.claude/skills/bootstrap/templates/task.md`, with the next free `B#`. Acceptance always includes a regression test (or, if untestable, a manual check the user can do).
6. **Affected tasks:** if a `done` task is wrong, ask the user before setting it `reopened` with `notes: see B<n>`. Usually the bug task alone is enough; reopen only if the task's own acceptance criteria no longer hold.
7. If the bug reveals a wrong fact, run `kb-update` too.
8. Add a **Change log** line, then reply with the B# ID, severity, the suspected cause and whether it jumps the queue. `critical` bugs come next; others go in normal order unless the user says otherwise.
