#!/usr/bin/env bash
# janitor.sh — suite artifact hygiene (design D10/D11, S0.7).
#
#   scripts/janitor.sh          # report, then clean what the rules allow
#   scripts/janitor.sh --dry    # report only
#
# Safety rules (D10) — this script would rather leave garbage than eat
# a meal that is not garbage:
#   - It touches ONLY /tmp paths matching the suite prefix or the
#     explicit legacy list below.
#   - Nothing younger than 24 h is removed.
#   - Nothing referenced by a live process (lsof) is removed.
#   - Processes are never killed silently: the roster prints first.
#
# target/ pruning (D11): `cargo sweep` when installed; otherwise this
# script only REPORTS target sizes — a janitor that guesses at build
# artifacts deletes someone's incremental cache (r1022's rsync --delete
# lesson, receiver side).
set -euo pipefail

DRY=0
[[ "${1:-}" == "--dry" ]] && DRY=1

say() { printf 'janitor: %s\n' "$*"; }

# ── 1. stale suite tmp dirs and known legacy prefixes ────────────────
LEGACY=(spg-gate-sqlx spg-relperf spg-r1047 spg-r1047s spg-sweep spg-suite-baseline)
CANDIDATES=()
while IFS= read -r p; do
    CANDIDATES+=("$p")
done < <(find /tmp -maxdepth 1 \( -name 'spg-suite-*' $(printf -- "-o -name %s* " "${LEGACY[@]}") \) 2>/dev/null || true)

for p in ${CANDIDATES[@]+"${CANDIDATES[@]}"}; do
    # Age gate: skip anything younger than 24 h.
    if [[ -n "$(find "$p" -maxdepth 0 -mtime -1 2>/dev/null)" ]]; then
        say "keep (young):    $p"
        continue
    fi
    # Live-reference gate.
    if lsof +D "$p" >/dev/null 2>&1; then
        say "keep (in use):   $p"
        continue
    fi
    if [[ "$DRY" == 1 ]]; then
        say "would remove:    $p"
    else
        say "removing:        $p"
        rm -rf "$p"
    fi
done

# ── 2. leaked suite processes — REPORT, kill only with the roster shown ─
LEAKS=$(pgrep -fl 'release/spg-server 127\.0\.0\.1:254[67][0-9]' 2>/dev/null || true)
if [[ -n "$LEAKS" ]]; then
    say "leaked suite-port servers:"
    printf '%s\n' "$LEAKS"
    if [[ "$DRY" == 0 ]]; then
        say "killing the roster above"
        pkill -f 'release/spg-server 127\.0\.0\.1:254[67][0-9]' || true
    fi
else
    say "no leaked suite-port servers"
fi
TAILS=$(pgrep -fl 'tail -n \+[01] -f /tmp/spg-' 2>/dev/null || true)
if [[ -n "$TAILS" ]]; then
    say "leaked log waiters:"
    printf '%s\n' "$TAILS"
    if [[ "$DRY" == 0 ]]; then
        say "killing the roster above"
        pkill -f 'tail -n \+[01] -f /tmp/spg-' || true
    fi
else
    say "no leaked log waiters"
fi

# ── 3. target/ — measure always, prune only via cargo sweep ──────────
if [[ -d target ]]; then
    say "target/ size: $(du -sh target 2>/dev/null | cut -f1)"
    if command -v cargo-sweep >/dev/null 2>&1; then
        if [[ "$DRY" == 1 ]]; then
            say "cargo sweep available (would prune stamps older than 14d)"
        else
            cargo sweep --time 14 2>/dev/null | tail -1 || true
        fi
    else
        say "cargo-sweep not installed — reporting only (D11: no guessing at build caches)"
    fi
fi

say "done$( [[ $DRY == 1 ]] && echo ' (dry run)' )"
