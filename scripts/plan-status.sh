#!/bin/bash
# Summarizes docs/plan.md: status counts, open work, and the next unblocked task.
# Usage: scripts/plan-status.sh [--short]
#
# Expected task format in plan.md:
#   ### T1.2 — Title          (or ### B3 — Title for bugs)
#   - status: todo            todo|in_progress|in_review|done|blocked|reopened
#   - deps: T0.2, T1.1        (or "-" for none)

root="$(cd "$(dirname "$0")/.." && pwd)"
plan="$root/docs/plan.md"

if [ ! -f "$plan" ]; then
  echo "No docs/plan.md yet. Run /bootstrap to create the plan and knowledge base."
  exit 0
fi

awk -v short="$1" '
function flush() {
  if (id != "") { ids[++n] = id; title[id] = ttl; st[id] = (s == "" ? "todo" : s); dp[id] = d }
  id = ""; ttl = ""; s = ""; d = ""
}
/^### [TB][0-9]/ {
  flush()
  id = $2
  ttl = $0; sub(/^### [^ ]+ *(—|-)? */, "", ttl)
  next
}
/^##? / { flush() }
id != "" && /^- status:/ { s = $3 }
id != "" && /^- deps:/   { d = $0; sub(/^- deps: */, "", d) }
END {
  flush()
  for (i = 1; i <= n; i++) count[st[ids[i]]]++

  # Next task: first reopened bug/task, then first todo, whose deps are all done.
  next_id = ""
  for (pass = 1; pass <= 2 && next_id == ""; pass++) {
    want = (pass == 1 ? "reopened" : "todo")
    for (i = 1; i <= n && next_id == ""; i++) {
      t = ids[i]
      if (st[t] != want) continue
      ok = 1
      m = split(dp[t], parts, /[ ,]+/)
      for (j = 1; j <= m; j++) {
        p = parts[j]
        if (p == "" || p == "-" || p == "none") continue
        if (st[p] != "done") { ok = 0; break }
      }
      if (ok) next_id = t
    }
  }

  summary = sprintf("%d tasks · %d done · %d todo · %d in progress · %d in review · %d blocked · %d reopened",
    n, count["done"], count["todo"], count["in_progress"], count["in_review"], count["blocked"], count["reopened"])
  nxt = (next_id == "" ? "none unblocked" : next_id " — " title[next_id])

  if (short == "--short") {
    printf "Controlz plan: %s · next: %s\n", summary, nxt
    for (i = 1; i <= n; i++) if (st[ids[i]] == "in_progress" || st[ids[i]] == "in_review")
      printf "WARNING: %s is %s from a previous session; check git before continuing.\n", ids[i], st[ids[i]]
    exit
  }

  print summary
  print "next: " nxt
  print ""
  for (i = 1; i <= n; i++) {
    t = ids[i]
    if (st[t] == "done" || st[t] == "todo") continue
    printf "  %-6s %-12s %s\n", t, st[t], title[t]
  }
}
' "$plan"
