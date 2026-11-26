# Controlz

macOS menu-bar control center for LG appliances: a Rust core (`core/`, uniffi) and a thin SwiftUI shell (`app/`).

- Sessions start as the **coordinator** agent (`.claude/settings.json` → `"agent"`). It owns the plan and delegates all code to subagents.
- Sources of truth: `docs/plan.md` (what to do) and `docs/knowledge_base.md` (what is true). Read them before acting; they win over memory.
- Workflow: one task per cycle → implementer brief → user approves → implement → reviewer → fix (≤2 rounds) → overview to the user → close + commit only when the user says so (D14). See `.claude/skills/implement-task`.
- Never put the ThinQ token or TV client-key in code, fixtures, logs or docs.
- Agents that edit files show the user a brief first and start only after approval (D16). Nothing outside the repo (installs, shell profiles, global config) changes without the user's prior approval of that exact change; Claude's scratchpad is exempt (D15).
- The repo is public. Private details (device IPs, MACs, serials, ThinQ device IDs, account email) go only in `docs/local.md` (gitignored); the knowledge base just says "see docs/local.md". Fixtures use fake values. The `.githooks/pre-commit` hook enforces this — never bypass it with `--no-verify`.
