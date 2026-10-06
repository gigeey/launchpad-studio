# Unsigned Windows preview

The manual [Windows preview workflow](../.github/workflows/windows-preview.yml) builds
an x64 NSIS `*-setup.exe` on `windows-2022` using Rust MSVC and Node 22. It runs
`cargo test --locked -p ao-process --lib --target x86_64-pc-windows-msvc`, then the
existing Tauri command, which invokes `npm run build` for the frontend.

This is an experimental preview, not a verified Windows release. Windows runtime
smoke testing remains necessary. The installer has no Authenticode signature and
may display Windows SmartScreen warnings. It is uploaded only as a GitHub Actions
artifact (14-day retention), never published to Releases.

## Prerequisites on the destination PC

Install [Git for Windows](https://git-scm.com/download/win), including Git Bash.
Bash tools, shell snapshots, hooks, and workflow scripts require it; the installer
does not bundle Git. Standard Git installation locations are detected. For a custom
installation, set the user environment variable `LAUNCHPAD_GIT_BASH` to the full
path to `bin/bash.exe` (for example `C:\Tools\Git\bin\bash.exe`), then restart
Launchpad Studio. WSL's System32 `bash.exe` launcher is not supported. Install the
provider CLIs you want to use separately; npm `.cmd` shims are resolved by the app.
The NSIS installer uses Tauri's default WebView2 bootstrapper when needed, which
can require internet access.

## Build and obtain the installer

The workflow and portability fixes must first be committed and pushed to a branch
with the owner's authorization. GitHub manual dispatch requires this workflow to
exist on the default branch; thereafter select the desired pushed branch in the
Actions UI. A dispatch cannot access uncommitted files on your Mac.

1. Open **Actions → Windows unsigned preview → Run workflow**, select the branch
   containing all Windows fixes, and run it.
2. Wait for the entire job to succeed. Open that run's **Artifacts** section and
   download `launchpad-studio-windows-x64-unsigned-<commit SHA>`.
3. Extract the ZIP to obtain the `*-setup.exe`. The build log includes its SHA256
   and verifies that the installer is unsigned.

Equivalent CLI commands, after the workflow is available:

```sh
gh workflow run windows-preview.yml --repo gigeey/launchpad-studio --ref <pushed-branch>
gh run list --repo gigeey/launchpad-studio --workflow windows-preview.yml --limit 5
gh run watch <run-id> --repo gigeey/launchpad-studio --exit-status
gh run download <run-id> --repo gigeey/launchpad-studio --dir windows-preview
```

Check the run's commit SHA matches the intended source before downloading. A failed
run is not evidence of a working installer; inspect its build logs before retrying.

For a local build on Windows, install stable Rust with the MSVC target, Visual
Studio C++ build tools, and a supported Node version, then run:

```sh
cd frontend
npm ci
npm run tauri -- build --target x86_64-pc-windows-msvc --bundles nsis --no-sign --config src-tauri/tauri.conf.windows-preview.json -- --locked
```

Output is under `target/x86_64-pc-windows-msvc/release/bundle/nsis/` at repository
root. This Mac cannot run the Windows-native MSVC build; a native Windows runner
is required for this workflow.

## Preview configuration isolation

The explicit [preview overlay](../frontend/src-tauri/tauri.conf.windows-preview.json)
sets `bundle.createUpdaterArtifacts` to `false`, selects NSIS, uses a separate app
identifier and product name, and clears updater endpoints. It uses a normal opaque
Windows window. It needs no signing or release secrets. It does not change the base
Tauri config, tools config, or the macOS release pipeline. Automatic updates are
unavailable for this preview; install later previews manually.

References: [Tauri Windows installers](https://v2.tauri.app/distribute/windows-installer/)
and [Tauri configuration](https://v2.tauri.app/reference/config/).
