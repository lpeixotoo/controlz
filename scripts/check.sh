#!/bin/bash
# Workspace quality gate: fmt, clippy (-D warnings) and tests.
# Uses rustup's toolchain (pinned by rust-toolchain.toml), never Homebrew's cargo.
# Usage: scripts/check.sh
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
