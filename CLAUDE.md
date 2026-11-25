# Controlz

macOS menu-bar control center for LG appliances: a Rust core (`core/`, uniffi) and a thin SwiftUI shell (`app/`).

- Sessions start as the **coordinator** agent (`.claude/settings.json` → `"agent"`). It owns the plan and delegates all code to subagents.
- Sources of truth: `docs/plan.md` (what to do) and `docs/knowledge_base.md` (what is true). Read them before acting; they win over memory.
- Workflow: one task per cycle → implementer → reviewer → fix (≤2 rounds) → done → commit. See `.claude/skills/implement-task`.
- Never put the ThinQ token or TV client-key in code, fixtures, logs or docs.
- The repo is public. Private details (device IPs, MACs, serials, ThinQ device IDs, account email) go only in `docs/local.md` (gitignored); the knowledge base just says "see docs/local.md". Fixtures use fake values. The `.githooks/pre-commit` hook enforces this — never bypass it with `--no-verify`.
