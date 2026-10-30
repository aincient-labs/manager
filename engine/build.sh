#!/usr/bin/env bash
# Build the engine image locally as atelier-engine:dev (never pushed; the
# opt-in compose file uses it with pull_policy: never).
set -euo pipefail
cd "$(dirname "$0")/.."
docker build -f engine/Dockerfile -t atelier-engine:dev .
