#!/usr/bin/env bash
# Tear down the competitor stack. No-op when already down.
set -euo pipefail
cd "$(dirname "$0")/.."
docker compose down -v
