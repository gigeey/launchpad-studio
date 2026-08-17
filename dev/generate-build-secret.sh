#!/usr/bin/env bash
# Generates a random 64-character hex string for use as a build secret.
# Usage: ./dev/generate-build-secret.sh
# Output: A single line with the 64-char hex secret.
#
# Store the output as VITE_BUILD_SECRET in your environment before building.

set -euo pipefail

openssl rand -hex 32
