# Task format

Every task and bug in `docs/plan.md` uses exactly this shape. `scripts/plan-status.sh` parses the heading, `status` and `deps` lines, so keep them on one line each.

```markdown
### T1.2 — webOS connect + pairing
- status: todo
- deps: T0.2, T1.1
- owner: implementer
- files: core/src/webos.rs, core/tests/webos.rs
- kb: TV / webOS protocol; Conventions
- design: -
- approach: tokio-tungstenite client; native-tls connector that accepts the TV's self-signed cert; reader task routes replies by id
- acceptance:
  - [ ] Connects to wss://<ip>:3001 first and falls back to ws://<ip>:3000
  - [ ] Retries registration with the unsigned manifest when the signed one is rejected
  - [ ] Returns the client-key on first pairing; reuses a stored key without prompting
  - [ ] Unit tests cover the registration message and both response paths (fixtures)
- notes: needs the user at the TV to accept the pairing prompt for manual verification
```

Bug variant (lives under `## Bugs`):

```markdown
### B3 — Volume slider moves the wrong way
- status: todo
- severity: major
- deps: -
- owner: implementer
- affects: T1.3
- files: core/src/webos.rs
- kb: TV / webOS protocol
- design: -
- repro: 1. open popover 2. drag volume up 3. TV volume goes down
- acceptance:
  - [ ] Dragging up increases TV volume (regression test added)
- notes: reported 2026-11-25
```

Field rules:
- **ID**: `T<phase>.<n>` for tasks, `B<n>` for bugs. Never reuse an ID; dropped tasks keep their ID with `status: blocked` and a note, or are deleted after asking the user.
- **deps**: comma-separated IDs or `-`.
- **owner**: `implementer` or `designer`.
- **approach**: one line (a few sentences) with the intended technical design — libraries, structure, key mechanisms. Guidance for the implementer, not a contract; deviations are explained in the report.
- **acceptance**: 2–5 criteria, each checkable by reading code, running a test, or looking at a screenshot.
