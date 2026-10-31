#!/usr/bin/env bash
# The developer path (DECISIONS 0487): build the engine image locally as
# atelier-engine:dev (never pushed). Released Managers don't use it: each one
# carries the digest of ghcr.io/aincient-labs/atelier-engine published from
# its tag, and `atelier engine enable --experimental` pulls and pins that. Only
# a developer build of the Manager (no ATELIER_ENGINE_DIGEST at compile time)
# pins the image ID this tag names (pull_policy: never); rebuilding changes
# nothing on a stack until enable runs again.
set -euo pipefail
cd "$(dirname "$0")/.."
docker build -f engine/Dockerfile -t atelier-engine:dev .
