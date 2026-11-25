# Controlz

A macOS menu-bar control center for LG appliances: an **LG webOS TV** (local network) and an **LG air conditioner** (LG ThinQ Connect cloud API).

- **Rust core** (`core/`): every device protocol, exported to Swift with [uniffi](https://mozilla.github.io/uniffi-rs/).
- **SwiftUI shell** (`app/`): a native `MenuBarExtra` popover plus Settings. Secrets are stored in the Keychain.

> Status: planning. Nothing is runnable yet. See `docs/plan.md` once it exists.

## How this project is built

Development runs through Claude Code with a small team of agents. Two files on disk are the only sources of truth, so any number of sessions can pick up where the last one stopped.

| File | Holds | Edited by |
|---|---|---|
| `docs/plan.md` | Phases, tasks (`T#`), bugs (`B#`), statuses, deps, acceptance criteria, session log, change log | coordinator only |
| `docs/knowledge_base.md` | Device protocols, API facts, decisions (`D#`), conventions, gotchas, open questions | coordinator, plus appends from the implementer |
| `docs/design/*.md` | UI specs per screen | designer only |

### Agents (`.claude/agents/`)

| Agent | Role | Can edit |
|---|---|---|
| **coordinator** | Main session and entrypoint. Reads state, takes bugs, corrections and plan tweaks, and dispatches the others | `docs/`, `CLAUDE.md`, `README.md`, `.claude/` |
| **implementer** | Staff engineer (Rust-first, plus the Swift shell). Implements one task | code and tests; appends to the knowledge base |
| **reviewer** | Staff engineer reviewer. Read-only; runs fmt, clippy, tests; returns PASS/FAIL with ranked findings | nothing |
| **designer** | Staff macOS designer. Writes specs before UI tasks and reviews screenshots after | `docs/design/` |

### Skills (`.claude/skills/`)

**Planning:**

| Command | What it does |
|---|---|
| `/bootstrap [--rebuild]` | Interviews you, fills research gaps, and writes the knowledge base and the plan. Manual only |
| `/status` | Progress, in-flight/blocked work, next task, open questions |
| `/plan-adjust <change>` | Add, split, reorder or drop tasks; checks dependencies and logs the change |
| `/kb-update <fact>` | Records a fact, correction or decision and finds the tasks it affects |
| `/report-bug <what broke>` | Clarifies, optionally has the reviewer investigate, and creates a `B#` task |

**Execution:**

| Command | What it does |
|---|---|
| `/next` | Picks the next unblocked task (critical bugs and reopened tasks first) and runs it |
| `/implement-task <ID>` | Design spec if needed → implementer → reviewer → up to 2 fix rounds → done → commit |
| `/review [ID\|diff\|range]` | Runs the reviewer on its own |

**Reference** (loaded into agents automatically, not in the `/` menu): `task-contract`, `rust-standards`, `swift-shell`, `macos-design`.

You rarely need the slash commands. Talk to the coordinator in plain language ("what's next?", "the TV doesn't wake up", "my AC is a 2021 Dual Inverter") and it picks the right skill.

### Guardrails (hooks)

- The coordinator and designer are blocked from editing outside their folders (`.claude/hooks/guard-paths.sh`).
- After the implementer edits Rust, `cargo fmt` and `cargo check` run automatically and errors go straight back to it (`.claude/hooks/rust-check.sh`).
- Each session starts with a one-line plan summary and a warning about tasks left half-done (`scripts/plan-status.sh --short`).

## Getting started

```bash
cd controlz
claude              # starts as the coordinator (set in .claude/settings.json)
> /bootstrap        # first time: answers + plan
> next              # run the next task
> status            # where are we?
```

To talk to Claude without the coordinator, run `claude --agent general-purpose`, or temporarily remove `"agent"` from `.claude/settings.json`.

### What you will need

- An LG ThinQ **personal access token** from https://connect-pat.lgthinq.com (signed in with your ThinQ account).
- The TV's IP and MAC address, with **Turn on via Wi-Fi** enabled on the TV (for power-on).
- Rust (via `rustup`) and Xcode command-line tools (Swift 6).

## Repository layout

```
CLAUDE.md                  short project brief for Claude
README.md
docs/
  plan.md                  what to do (created by /bootstrap)
  knowledge_base.md        what is true
  design/                  UI specs
core/                      Rust crate (controlz_core)
app/                       SwiftUI menu-bar app (later phase)
scripts/plan-status.sh     plan parser used by hooks and /status
.claude/
  settings.json            default agent, permissions, SessionStart hook
  agents/                  coordinator, implementer, reviewer, designer
  skills/                  planning, execution and reference skills
  hooks/                   guard-paths.sh, rust-check.sh
```
