#!/bin/bash
# Workspace quality gate (KB Tooling, D13): the single definition of "green".
# Steps: fmt, clippy (-D warnings), nextest, doctests, cargo-deny, cargo-machete.
# Stops at the first failing step. Installs nothing: if a tool is missing it
# prints the exact install command and exits 1.
# Uses rustup's toolchain (pinned by rust-toolchain.toml), never Homebrew's cargo.
# --locked makes cargo fail instead of rewriting Cargo.lock.
# Usage: scripts/check.sh
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."

missing=()
for tool in nextest deny machete; do
  command -v "cargo-$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "check.sh: missing cargo tools: ${missing[*]/#/cargo-}" >&2
  echo "Install them once with:" >&2
  echo '  export PATH="$HOME/.cargo/bin:$PATH"; cargo install --locked cargo-nextest@0.9.152 cargo-deny@0.22.1 cargo-machete@0.10.1' >&2
  exit 1
fi

step() {
  local name=$1; shift
  echo "check.sh: $name"
  if ! "$@"; then
    echo "check.sh: $name failed" >&2
    exit 1
  fi
}

step fmt      cargo fmt --all --check
step clippy   cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
step nextest  cargo nextest run --locked --workspace --all-features
step doctests cargo test --doc --locked --workspace --all-features
step deny     cargo deny --locked --workspace --all-features check --hide-inclusion-graph
step machete  cargo machete

echo "check.sh: all green"
