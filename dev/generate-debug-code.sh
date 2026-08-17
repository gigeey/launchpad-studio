#!/usr/bin/env bash
# Generates the daily debug code for a given build secret and app version.
# Usage: ./dev/generate-debug-code.sh <build_secret> <app_version>
#
# The code is HMAC-SHA256(version + YYYY-MM-DD, secret) truncated to 6 digits.
# The browser-side validator in frontend/src/lib/debugUnlock.ts implements the
# same algorithm against the Web Crypto API; the two must agree.

set -euo pipefail

if [ $# -ne 2 ]; then
  echo "Usage: $0 <build_secret> <app_version>" >&2
  exit 1
fi

BUILD_SECRET="$1"
APP_VERSION="$2"
TODAY=$(date -u +"%Y-%m-%d")

MESSAGE="${APP_VERSION}${TODAY}"

# Compute HMAC-SHA256, extract hex digest, convert to decimal, take last 6 digits
HEX=$(echo -n "$MESSAGE" | openssl dgst -sha256 -hmac "$BUILD_SECRET" | sed 's/^.* //')

# Take first 8 hex chars, convert to decimal, mod 1000000, zero-pad to 6 digits
DECIMAL=$(printf "%d" "0x${HEX:0:8}")
CODE=$(printf "%06d" $((DECIMAL % 1000000)))

echo "$CODE"
