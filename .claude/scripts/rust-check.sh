#!/bin/bash
# Read-only Rust quality check, run by the reviewer agent (not a hook).
# Uses rustup's toolchain (pinned by rust-toolchain.toml), never Homebrew's cargo.
# --locked makes cargo fail instead of rewriting Cargo.lock.
# Usage: .claude/scripts/rust-check.sh
# Exit code: 0 if every check passes, 1 otherwise.

export PATH="$HOME/.cargo/bin:$PATH"
root="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"

# Workspace root once T0.2 adds it; core/ until then.
if [ -f "$root/Cargo.toml" ]; then dir="$root"; elif [ -f "$root/core/Cargo.toml" ]; then dir="$root/core"; else
  echo "rust-check: no Cargo.toml found, nothing to check"; exit 0
fi
cd "$dir" || exit 1

echo "rust-check: $(cargo --version) in ${dir#"$root"/}"
fail=0
run() {
  local name=$1; shift
  if out=$("$@" 2>&1); then
    echo "  ✓ $name"
  else
    echo "  ✗ $name"
    printf '%s\n' "$out" | tail -40 | sed 's/^/      /'
    fail=1
  fi
}
run "fmt"    cargo fmt --all --check
run "clippy" cargo clippy --locked --all-targets -- -D warnings
run "test"   cargo test --locked

exit $fail
