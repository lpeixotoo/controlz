#!/bin/bash
# PostToolUse hook for the implementer: after editing a Rust file under core/,
# format the crate and run `cargo check`. Errors are fed back to the agent.

input=$(cat)
file=$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty')
root="${CLAUDE_PROJECT_DIR:-$(pwd)}"
rel="${file#"$root"/}"

case "$rel" in
  core/*.rs|core/Cargo.toml) ;;
  *) exit 0 ;;
esac
[ -f "$root/core/Cargo.toml" ] || exit 0

cd "$root/core" || exit 0
cargo fmt --quiet 2>/dev/null
if ! out=$(cargo check --quiet --all-targets --message-format short 2>&1); then
  echo "cargo check failed after editing $rel:" >&2
  printf '%s\n' "$out" | grep -E '^(error|warning: unused)|^[^ ].*: error' | head -30 >&2
  exit 2
fi
exit 0
