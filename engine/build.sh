#!/usr/bin/env bash
# Build the engine image locally as atelier-engine:dev (never pushed). A
# stack runs it only after `atelier engine enable --experimental`, which pins
# the image ID this tag names then (pull_policy: never); rebuilding changes
# nothing on a stack until enable runs again.
set -euo pipefail
cd "$(dirname "$0")/.."
docker build -f engine/Dockerfile -t atelier-engine:dev .
