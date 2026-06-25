#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "Building Debian packages for x86_64 (amd64) and aarch64 (arm64)..."

# Build for amd64 (x86_64)
"$SCRIPT_DIR/build_deb.sh" amd64 x86_64-unknown-linux-gnu

# Build for arm64 (aarch64)
"$SCRIPT_DIR/build_deb.sh" arm64 aarch64-unknown-linux-gnu

echo "All Debian packages built successfully!"
