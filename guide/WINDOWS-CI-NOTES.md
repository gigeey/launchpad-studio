# Windows installer CI verification — 2026-10-06

Task: 9c1d8be2-afd3-4e0a-b6ca-b932425c5c6a
Repository: /Users/ebongandrew/dev/launchpad_studio-tools

## Delivered

- Added `.github/workflows/windows-preview.yml`: manual workflow_dispatch only, Windows 2022 native x64 MSVC, Node 22, npm ci, locked Windows ao-process tests, existing npm/Tauri build command with --no-sign and NSIS, explicit unsigned Authenticode verification and SHA256 log, upload-artifact with 14-day retention. Read-only contents permission; no secrets, signing, release publishing, or source upload steps.
- Added `frontend/src-tauri/tauri.conf.windows-preview.json`: NSIS/current-user installer, createUpdaterArtifacts=false, separate preview product/identifier, opaque decorated Windows window, empty updater endpoints. Base and tools Tauri configs and macOS runner configuration unchanged.
- Added `guide/WINDOWS-PREVIEW.md`: Git for Windows/Git Bash prerequisite, custom LAUNCHPAD_GIT_BASH path, WebView2 internet bootstrap, Actions/gh build and download instructions, local Windows build command, preview limitations. Updated README Windows support statement honestly.
- Read prior portability findings and inspected their diffs, including npm executable resolution. Preserved all prior changes. No applicable AGENTS.md found in repository/ancestor paths (node_modules vendor instructions do not apply).

## Validation

PASS: actionlint .github/workflows/windows-preview.yml.
PASS: merged base+preview config validated against installed Tauri CLI JSON schema; asserted NSIS, disabled updater artifacts and empty endpoints, base v1Compatible unchanged.
PASS: npm run build (TypeScript + Vite); existing dynamic-import/chunk-size warnings only.
PASS: git diff --check.
PASS: dev/check-doc-links.sh, 62 relative links across 28 Markdown files at validation time.
PASS: git diff --exit-code for base/tools Tauri configs and .cargo/config.toml.
Confirmed installed Tauri CLI accepts --no-sign and passthrough Cargo arguments (-- --locked).
Validation tooling stayed in /tmp; no dependency or lockfile changes from this task. Initial Python 3.8 jsonschema latest installation failed in a transitive build; jsonschema 4.17.3 installed in isolated temporary venv and completed schema validation.
Prior task already passed macOS backend checks/tests and Windows ao-process GNU cross-check; full Windows backend cross-check was blocked by missing MinGW C compiler. No repeated full Rust tests for workflow/docs-only changes.

## Artifact status and required next action

NO .exe produced or retrieved. gh is authenticated as gigeey. Remote has only ci, cla-test, cla, docs-links and third-party-notices workflows; no Windows packaging/artifact workflow. Inspected remote ci definition: macOS/Linux runners, no Windows installer upload. Local HEAD ac33c7a45a2a147a155cac80e2fdaf49fd8fd3c4 differs from remote main ec007f652e8a2407d790b7e35f86a87673a6726f, and portability/workflow changes are uncommitted. Existing workflows cannot build this local source; no dispatch was attempted.

Required: owner authorization to commit and push the complete portability and CI changes (reconcile remote main first), make the workflow available on default branch, then dispatch Windows unsigned preview on the intended pushed ref and download the successful run artifact. This task did not commit, push, sign or publish. The Mac has no native Windows MSVC build environment; native installer and runtime success remain unverified until Windows CI runs. Completing preparation does not mean the requested installer exists.
