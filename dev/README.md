# dev/

Scripts that a contributor or CI actually needs. Seven files, no build system,
no hidden state — each one is readable top to bottom in under a minute.

| File | What it does | Who runs it |
|---|---|---|
| `check-doc-links.sh` | Verifies every relative link in every shipping Markdown file resolves to a file that will actually be published | CI, on every push and pull request |
| `generate-third-party-notices.mjs` | Rewrites the dependency inventory in `THIRD-PARTY-NOTICES.md` from `Cargo.lock` and `package-lock.json`, and refuses licences this project does not ship under | Whoever changes a dependency; CI checks the result |
| `generate-third-party-notices.test.mjs` | Tests the above, mostly by feeding it licences it must reject | CI, before it trusts the check; `node --test dev/*.test.mjs` locally |
| `dev-codesign-runner.sh` | Re-signs the dev binary with a stable identity so macOS Keychain grants survive a rebuild | `cargo run` / `cargo test`, automatically, on macOS |
| `dev-entitlements.plist` | Grants `get-task-allow` so a debugger can attach to the re-signed dev binary | `dev-codesign-runner.sh` |
| `generate-build-secret.sh` | Prints a random 64-char hex string for `VITE_BUILD_SECRET` | Whoever produces a packaged build |
| `generate-debug-code.sh` | Computes the daily debug-panel unlock code from a secret and a version | Whoever needs to open the debug panel on a packaged build |

## `check-doc-links.sh`

Run it yourself with `./dev/check-doc-links.sh`. Exit 0 means every relative
link resolves; exit 1 prints each broken one with the path it resolved to.

It checks link targets against the *shipping set* rather than against the
filesystem, because this repository is published as a fresh `git init` plus
`git add -A` — which makes `.gitignore` the sole authority on what a reader
will see. A linked file can exist on disk, open fine in an editor, and still
404 for everyone else. The script's header comment explains the two `git`
flags that make this work and why one of them is load-bearing.

## `generate-third-party-notices.mjs`

```bash
dev/generate-third-party-notices.mjs --check   # is THIRD-PARTY-NOTICES.md accurate?
dev/generate-third-party-notices.mjs           # make it accurate
node --test dev/*.test.mjs                     # is the generator itself right?
```

Run it after changing any dependency in either ecosystem. Exit 0 means the file
is current and every licence in the tree is one this project can ship; exit 1
names what changed or what needs a human.

It reads `cargo metadata --locked` and `frontend/package-lock.json` — both
resolved for every target platform, not for the host. That is the whole reason
it exists in this form: an earlier version of the inventory was built by reading
the installed `node_modules/`, which on macOS silently omitted eighty packages
that a Linux contributor has on disk, eleven of them under MPL-2.0.

`cargo metadata` needs the network the first time it runs on a machine, because
reading a crate's declared licence means having downloaded that crate — including
the Linux- and Windows-only ones nothing here ever builds. The npm half needs
nothing but the lockfile.

It rewrites two regions of `THIRD-PARTY-NOTICES.md` and leaves the rest alone.
The prose above the inventory — which obligations we carry, which alternative we
elect for a dual-licensed package — is judgement, not data, and no generator
should be writing it. The script locates those two regions by the headings
already in the file rather than by inserted markers, and fails if it cannot find
exactly one of each, so a rewrite of the wrong region is not a thing it can do
quietly.

Two claims in that prose are checked on every run: that nothing in the tree is
under the GPL, AGPL, SSPL, BUSL, CDDL, EPL, OSL, EUPL, the Commons Clause or the
Elastic License, and that every package which is not plainly permissive is named
in the "Obligations we carry" table. A new dependency that breaks either one
fails the script instead of reaching a reader. The `REVIEWED` list in the script
is the machine-readable half of that table; an entry there for a package that is
no longer a dependency also fails, because the prose would then be describing an
obligation this project does not carry.

## `dev-codesign-runner.sh`

Wired in via `.cargo/config.toml` as the cargo target runner for both macOS
triples, so it sits in front of every `cargo run` and `cargo test`.

**It does nothing unless you set `LAUNCHPAD_DEV_SIGNING_IDENTITY`.** Unset, it
execs the binary and exits. No certificate is required to build, run, or test
this project.

It exists because the app stores provider API keys in the macOS Keychain
(`keyring` with the `apple-native` feature), and macOS ties Keychain
access-control decisions to a binary's code signature. `cargo run` produces an
ad-hoc signature whose hash changes on every rebuild, so without a stable
identity every rebuild looks like a brand-new app and re-triggers every
keychain prompt. Opting in costs one exported variable:

```bash
security find-identity -v -p codesigning   # list your development identities
export LAUNCHPAD_DEV_SIGNING_IDENTITY="Apple Development: Your Name (TEAMID)"
```

`guide/DEVELOPING.md` covers this in the macOS setup section.

## What is deliberately not here

**The packaging and release pipeline.** Building the signed, notarized,
distributable app is not part of this repository. Those scripts carry a
Developer ID identity, an Apple notarization credential flow, and a hard
coupling to a specific release-hosting repository — none of which is useful to
someone building from source, and some of which should not be public at all.

The practical consequences, stated plainly so nothing is a surprise:

- `VITE_BUILD_DATE` and `VITE_BUILD_SECRET` are unset in a source build. Both
  debug-panel gates in `frontend/src/lib/debugUnlock.ts` fail closed as a
  result, so the debug panel is off. That is the intended default.
- The in-app update check points at a release feed this repository does not
  publish. It fails open — a failed fetch leaves the app in the `allowed`
  state (`frontend/src/App.tsx`), so the app runs normally; the update banner
  and download button simply have nothing to offer.

Building an unsigned local app bundle still works: `npm run tauri build` in
`frontend/`. It will not be notarized, so macOS Gatekeeper will warn on first
launch.
