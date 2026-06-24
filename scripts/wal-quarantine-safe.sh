#!/usr/bin/env bash
# wal-quarantine-safe.sh — preserve-not-delete WAL quarantine
#
# Use when an spg-embedded process cannot boot and you need to
# unblock it WITHOUT losing the WAL records.
#
# What it does (see docs/WAL-QUARANTINE-RECOVERY.md for the full
# procedure including the recovery step):
#
#   1. cp base.spg → base-before-quarantine-<ts>.spg
#   2. mv .wal dir → wal-quarantine-<ts>/
#   3. rm .lock dir
#
# What it does NOT do:
#
#   - Does NOT stop your spg process. You must do that first
#     (docker compose stop / systemctl stop / SIGKILL pid).
#   - Does NOT start your spg process. Start it after this script
#     completes successfully.
#   - Does NOT recover the quarantined WAL records. See Step 6
#     of docs/WAL-QUARANTINE-RECOVERY.md for the recovery
#     procedure using `spg pitr-restore`.
#
# Usage:
#   scripts/wal-quarantine-safe.sh <db_path>
#
# Example:
#   docker compose stop spg
#   scripts/wal-quarantine-safe.sh /data/spg/mailrs.spg
#   docker compose start spg
#   # ... once stable, see Step 6 in docs/WAL-QUARANTINE-RECOVERY.md
#
# Exit codes:
#   0  — quarantine OK (or no-op, the WAL was already clean)
#   1  — preservation step (cp / mv) failed; database not touched
#   2  — bad arguments

set -euo pipefail

if [[ $# -lt 1 ]]; then
    cat >&2 <<'EOF'
usage: wal-quarantine-safe.sh <db_path>

Preserves WAL chunks + base snapshot for later recovery, then clears
the live data so spg can boot. Stop the spg process FIRST.

See docs/WAL-QUARANTINE-RECOVERY.md for the full procedure.
EOF
    exit 2
fi

DB_PATH="$1"
TS="$(date -u +%Y%m%dT%H%M%SZ)"

# Resolve paths the same way spg-embedded does.
BASE_FILE="${DB_PATH}"
WAL_DIR="${DB_PATH}.wal"
LOCK_DIR="${DB_PATH}.lock"

# Sanity: refuse to act on a path that has nothing to quarantine.
if [[ ! -f "${BASE_FILE}" && ! -d "${WAL_DIR}" ]]; then
    echo "no spg database at ${DB_PATH}: neither ${BASE_FILE} nor ${WAL_DIR} exists" >&2
    exit 0
fi

# Step 2 — preserve the base snapshot. Only if it exists; on a brand-new
# install with no checkpoints yet, the base file may not exist.
if [[ -f "${BASE_FILE}" ]]; then
    BASE_BACKUP="${BASE_FILE}.base-before-quarantine-${TS}"
    echo "preserving base snapshot: ${BASE_FILE} → ${BASE_BACKUP}" >&2
    cp -p "${BASE_FILE}" "${BASE_BACKUP}"
else
    echo "base snapshot does not exist (yet); nothing to preserve" >&2
fi

# Step 3 — move (not remove) the WAL directory.
if [[ -d "${WAL_DIR}" ]]; then
    WAL_QUARANTINE="${WAL_DIR}.quarantine-${TS}"
    echo "moving WAL: ${WAL_DIR} → ${WAL_QUARANTINE}" >&2
    mv "${WAL_DIR}" "${WAL_QUARANTINE}"
elif [[ -f "${WAL_DIR}" ]]; then
    # Legacy single-file layout
    WAL_QUARANTINE="${WAL_DIR}.quarantine-${TS}"
    echo "moving WAL (legacy single-file): ${WAL_DIR} → ${WAL_QUARANTINE}" >&2
    mv "${WAL_DIR}" "${WAL_QUARANTINE}"
else
    echo "no WAL to quarantine (${WAL_DIR} does not exist)" >&2
fi

# Step 4 — remove the lock directory.
if [[ -d "${LOCK_DIR}" ]]; then
    echo "clearing lock: ${LOCK_DIR}" >&2
    rm -rf "${LOCK_DIR}"
elif [[ -f "${LOCK_DIR}" ]]; then
    # Legacy single-file layout
    rm -f "${LOCK_DIR}"
fi

echo "quarantine complete." >&2
echo "" >&2
echo "Recovery files (preserved for Step 6 of docs/WAL-QUARANTINE-RECOVERY.md):" >&2
[[ -f "${BASE_BACKUP:-/nonexistent}" ]] && echo "  base snapshot:    ${BASE_BACKUP}" >&2
[[ -d "${WAL_QUARANTINE:-/nonexistent}" || -f "${WAL_QUARANTINE:-/nonexistent}" ]] && \
    echo "  quarantined WAL:  ${WAL_QUARANTINE}" >&2
echo "" >&2
echo "You may now start the spg process (docker compose start spg)." >&2
echo "Recovery of the quarantined WAL is OPTIONAL — see Step 6 of" >&2
echo "docs/WAL-QUARANTINE-RECOVERY.md for the spg pitr-restore loop." >&2
