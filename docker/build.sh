#!/usr/bin/env bash
set -euo pipefail

# Rebuilds harness-hat Docker images.
# Always rebuilds the base first, then language images in parallel.
# Run this after any changes to harness-hat-base.dockerfile or a language dockerfile.
#
# Usage:
#   ./docker/build.sh              # rebuild base + all language images
#   ./docker/build.sh go python    # rebuild base + specific images only

cd "$(dirname "$0")/.."

IMAGES=(csharp default go kotlin php python rust typescript)

# Optionally limit to specific images: ./docker/build.sh go python
if [ $# -gt 0 ]; then
    IMAGES=("$@")
fi

echo "==> Building harness-hat-base:local"
docker build -t harness-hat-base:local -f docker/harness-hat-base.dockerfile docker/

echo "==> Building language images in parallel: ${IMAGES[*]}"
pids=()
for img in "${IMAGES[@]}"; do
    docker build -t "harness-hat-${img}:local" -f "docker/${img}.dockerfile" docker/ &
    pids+=($!)
done

failed=0
for i in "${!pids[@]}"; do
    if ! wait "${pids[$i]}"; then
        echo "ERROR: harness-hat-${IMAGES[$i]}:local failed" >&2
        failed=1
    fi
done

[ "$failed" -eq 0 ] && echo "==> All images built successfully"
exit "$failed"
