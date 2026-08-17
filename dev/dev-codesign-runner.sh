#!/bin/bash
# Cargo target runner: signs the Tauri dev binary with a stable identity before
# running it, so macOS Keychain "Always Allow" grants survive rebuilds.
#
# Background: `cargo run`/`tauri dev` link an ad-hoc-signed binary whose
# signature hash changes on every rebuild (arm64 requires SOME signature to
# execute at all, so the linker applies one automatically). macOS ties
# Keychain access-control decisions to the code signature, so every rebuild
# looks like a brand-new app and re-triggers every keychain prompt. Re-signing
# with the same identity on every run keeps the app's identity stable across
# rebuilds.
#
# Wired via .cargo/config.toml's `target.<triple>.runner`, which is how
# `cargo run` (and therefore `tauri dev`) executes the binary it just built.
# This does NOT affect `cargo build` alone, so `tauri build` and any release
# build (neither of which invokes `cargo run`) are untouched.
#
# Set LAUNCHPAD_DEV_SIGNING_IDENTITY to your Apple Development identity to turn
# this on, e.g.
#
#   export LAUNCHPAD_DEV_SIGNING_IDENTITY="Apple Development: Your Name (TEAMID)"
#
# `security find-identity -v -p codesigning` lists the ones you have. Use a
# development identity, not a Developer ID one. This deliberately does NOT read
# APPLE_SIGNING_IDENTITY: that is Tauri's convention for the Developer ID
# identity used to sign distributable builds, and a local dev loop must never
# sign with it.
#
# Unset, the script does nothing but exec the binary. That is deliberate: the
# alternative is a baked-in default identity, which only exists on one
# machine and makes every other contributor's first `tauri dev` print a
# codesign failure naming a stranger's certificate. The cost of opting in is
# one exported variable; the cost of the default was a confusing error for
# everyone who is not its owner.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
IDENTITY="${LAUNCHPAD_DEV_SIGNING_IDENTITY:-}"

BIN="$1"
shift

case "${IDENTITY:+set}:$(basename "$BIN")" in
  set:launchpad-studio)
    # No --options runtime: hardened runtime without get-task-allow would
    # block lldb/Xcode from attaching to the dev binary. The entitlements
    # file below grants get-task-allow so debugging keeps working.
    ERR_LOG="$(mktemp)"
    if ! codesign --force --sign "$IDENTITY" \
        --entitlements "$SCRIPT_DIR/dev-entitlements.plist" \
        "$BIN" 2>"$ERR_LOG"; then
      echo "warning: dev codesign with '$IDENTITY' failed, running ad-hoc-signed binary instead (Keychain prompts will repeat on rebuild):" >&2
      cat "$ERR_LOG" >&2
    fi
    rm -f "$ERR_LOG"
    ;;
esac

exec "$BIN" "$@"
