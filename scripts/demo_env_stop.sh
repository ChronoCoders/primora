#!/usr/bin/env bash
# Stops the Primora demo environment started by scripts/demo_env.sh.
set -uo pipefail

BACKEND_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$BACKEND_ROOT"

echo "Stopping Primora demo environment..."
pkill -f primora-node 2>/dev/null && echo "  stopped node-servers (all 4)" || true
pkill -f primora-verification 2>/dev/null && echo "  stopped verification-service" || true
pkill -f anvil 2>/dev/null && echo "  stopped anvil (both chains)" || true
docker compose stop postgres redis 2>/dev/null && echo "  stopped Postgres + Redis" || true
echo "Done. (Postgres/Redis containers are stopped, not removed; demo data persists for next start.)"
