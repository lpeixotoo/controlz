#!/bin/bash
# PreToolUse guard: blocks Edit/Write outside the allowed path prefixes.
# Usage (from an agent's hooks): guard-paths.sh <prefix> [<prefix> ...]
# Prefixes are relative to the project root; a prefix without a trailing
# slash matches that exact file (e.g. CLAUDE.md). The special token
# @scratchpad also allows Claude's per-session scratchpad directory
# (/private/tmp/claude-*/.../scratchpad/ or /tmp/claude-*/.../scratchpad/).

input=$(cat)
file=$(printf '%s' "$input" | jq -r '.tool_input.file_path // .tool_input.notebook_path // empty')
[ -z "$file" ] && exit 0

root="${CLAUDE_PROJECT_DIR:-$(pwd)}"
rel="${file#"$root"/}"

for prefix in "$@"; do
  case "$prefix" in
    @scratchpad)
      case "$file" in
        /private/tmp/claude-*/scratchpad/*|/tmp/claude-*/scratchpad/*)
          case "$file" in *..*) ;; *) exit 0 ;; esac ;;
      esac ;;
    */) case "$rel" in "$prefix"*) exit 0 ;; esac ;;
    *)  [ "$rel" = "$prefix" ] && exit 0 ;;
  esac
done

echo "Blocked: this agent may only edit: $*. Tried to edit '$rel'. Delegate code changes to the implementer subagent." >&2
exit 2
