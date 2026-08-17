# Tauri + React + Typescript

This template should help get you started developing with Tauri, React and Typescript in Vite.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Debug Panel Security

The debug panel (DevPanel) is secured behind a multi-layer activation system to prevent discovery by regular users while remaining accessible to developers debugging on remote machines.

### Architecture

1. **Hidden activation gesture** — Tap the version label in Settings 7 times within 3 seconds to reveal a hidden debug code input. This mirrors the Android developer mode Easter egg pattern.

2. **Daily rotating debug code** — The input accepts a 6-digit code computed from HMAC-SHA256 of the app version and current date, keyed with a per-version build secret. The code changes daily and is version-scoped, so a code from one version or day cannot be reused.

3. **100-day end-of-life** — The debug panel activation silently stops working 100 days after the app was built. The app itself continues to function normally; only the debug panel is affected.

4. **Session-only unlock** — A valid code sets an in-memory flag for the current session. The flag is never persisted. Closing the app requires re-authentication.

5. **Hotkey gating** — The `Cmd+Option+W` hotkey to toggle the DevPanel only works after the session has been unlocked via a valid debug code.

### Environment Variables

| Variable | Purpose |
|---|---|
| `VITE_BUILD_SECRET` | 64-character hex string used as the HMAC key. Embedded at build time by Vite. |
| `VITE_BUILD_DATE` | ISO date string (e.g., `2026-04-11`) of when the app was built. Used for the 100-day EOL check. |

Both variables are set before building and replaced by Vite at build time via `import.meta.env`.

### Generating a Build Secret

Run the script to generate a new random 64-character hex secret (paths in this
section are relative to the repository root, not to `frontend/`):

```bash
./dev/generate-build-secret.sh
```

Set this as `VITE_BUILD_SECRET` before building the app. Each version should have its own unique secret.

### Computing the Daily Debug Code

To get today's debug code for a specific version:

```bash
./dev/generate-debug-code.sh <build_secret> <app_version>
```

The script computes `HMAC-SHA256(version + YYYY-MM-DD, secret)`, takes the first 8 hex characters, converts to decimal, takes modulo 1,000,000, and zero-pads to 6 digits. The browser-side implementation in `src/lib/debugUnlock.ts` uses the Web Crypto API with the same algorithm.

### Key Files

| File | Role |
|---|---|
| `dev/generate-build-secret.sh` | Generates a random build secret |
| `dev/generate-debug-code.sh` | Computes the daily debug code from a secret and version |
| `src/lib/debugUnlock.ts` | Browser-side HMAC validation, session unlock state, and EOL check |
| `src/pages/SettingsView.tsx` | 7-tap gesture and debug code input UI |
| `src/main.tsx` | Hotkey gating (checks unlock state before toggling DevPanel) |

`VITE_BUILD_DATE` is set by the packaging pipeline, which is not part of this
repository (see [`dev/README.md`](../dev/README.md)). A build from source
therefore leaves it unset, and `debugUnlock.ts` treats an absent build date
as *expired*, not as "no expiry". Together with the absent `VITE_BUILD_SECRET`
that means the debug panel is disabled twice over in a source build — both
checks fail closed, which is the intended default.
