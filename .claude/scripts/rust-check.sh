#!/bin/bash
# Rust quality check for the implementer and reviewer agents (not a hook).
# Delegates to scripts/check.sh so there is one definition of the gate:
# fmt, clippy -D warnings, nextest, doctests, cargo-deny, cargo-machete,
# all with --locked (cargo fails instead of rewriting Cargo.lock).
# Uses rustup's toolchain (pinned by rust-toolchain.toml), never Homebrew's cargo.
# Usage: .claude/scripts/rust-check.sh
# Exit code: 0 if every check passes, 1 otherwise.

export PATH="$HOME/.cargo/bin:$PATH"
root="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"

echo "rust-check: $(cargo --version)"
exec "$root/scripts/check.sh"
