# Contributing to Launchpad Studio

Thanks for considering it. This document covers what is useful, how to get a
build running, and what will happen to your pull request.

Please also read the [Code of Conduct](CODE_OF_CONDUCT.md).

## Before you write code

**Open an issue first for anything non-trivial.** Not as a formality — this
project is pre-1.0, parts of it are being actively reshaped, and the honest
risk is that you spend a weekend on something that collides with work in
progress. A short issue saves you that.

You do not need to ask before:

- fixing a bug you can reproduce
- correcting documentation that is wrong
- adding a test for existing behaviour

## The Contributor License Agreement

**Your first pull request cannot be merged until you sign the
[CLA](CLA.md).**

Read it — it is short and the first section is plain English. The part worth
knowing before you invest time: the licence you grant is **sublicensable**,
which means Gigeey can distribute your contribution under other licences,
including commercial ones. You keep your own copyright and can use your work
anywhere else.

That is a real trade. If you would rather not make it, that is a reasonable
position and you can still help by filing good bug reports, improving docs
through issues, or reviewing design discussions.

### How signing works in practice

Nothing to email, nothing to print. Open your pull request and a check posts a
comment asking for a signature; reply with the exact sentence it gives you and
the check goes green. It is a one-time step.

Two details worth knowing, because they surprise people:

- **Everyone whose commits are in the pull request has to sign, not only whoever
  opened it.** A pull request carrying someone else's commits is carrying their
  copyright, and a check that only looked at the author would be claiming more
  coverage than it had.
- **Commits written with an email address that is not linked to a GitHub account
  fail the check.** There is no account to match a signature against, so the
  honest answer is "unknown" rather than "fine". Adding the address under
  Settings → Emails on GitHub fixes it retroactively.

Signatures are recorded in [`signatures/version1/cla.json`](signatures/version1/cla.json)
in this repository, so you can read your own entry. The implementation is
[`.github/cla.js`](.github/cla.js) — one file, no dependencies, and worth a look
if you would rather see what a bot does than trust it. Its tests are next to it
in [`.github/cla.test.js`](.github/cla.test.js).

A maintainer can waive the check with the `cla-not-required` label for cases
where it does not apply, such as an automated dependency bump. The waiver is
visible on the pull request rather than hidden in a config file.

## Getting a build running

Full instructions with the reasoning behind each prerequisite are in
**[guide/DEVELOPING.md](guide/DEVELOPING.md)**. The short version:

```bash
git clone https://github.com/gigeey/launchpad-studio.git
cd launchpad-studio/frontend
npm install
npm run tauri dev
```

You need Rust (stable), Node.js 20.19+ / 22.13+ / 24+, a C compiler toolchain,
and roughly **9 GB of free disk** for the first Rust build. `npm install`
checks your Node version and stops if it is too old.

## Running the tests

Both commands start from the repository root — the build steps above leave you
in `frontend/`.

```bash
cargo test --workspace --no-fail-fast   # Rust
cd frontend && npx vitest run           # Frontend
```

Five things will bite you if nobody tells you, so here they are in full.

### Use `--no-fail-fast`

Cargo stops at the first test *binary* that fails and prints a running tally
that looks like a complete one. A truncated report reads exactly like a pass
with fewer tests, so the flag is the difference between seeing every failure and
believing you have.

### On macOS, four tests fail unless you disable keychain access

The four tests in `crates/ao-engine-tools-cli/tests/cli_smoke.rs` fail on a
developer Mac. Two of them — `cli_smoke_one_turn_echo` and
`cli_smoke_multi_chunk_text_renders_with_single_prefix` — name the cause on the
spawned binary's stderr:

```
error: failed to load provider config from <tmp>/providers.toml:
secret vault error: keychain error: Platform secure storage failure:
The user name or passphrase you entered is not correct.
```

The other two, `cli_cancel_mid_stream_prints_cancelled` and
`cli_double_sigint_exits_with_code_130`, never print that. They wait on a
connection the exiting binary never makes, so they fail on a timeout that names
nothing:

```
stub server never received a connection — CLI may have crashed: Elapsed(())
```

Nothing is broken. Each test spawns the real CLI binary, which loads
`providers.toml` during startup (`crates/ao-engine-tools-cli/src/main.rs`);
that load opens the secret vault
(`crates/ao-engine-tools-provider-config/src/lib.rs`), and the vault reaches
the macOS Keychain. A `cargo build` dev binary is ad-hoc-signed with a signature
that changes on every rebuild, so any keychain grant you give it is void by the
next build. You get no dialog offering to re-grant it, because the CLI disables
interactive keychain prompts at startup
(`crates/ao-engine-tools-cli/src/main.rs`) — deliberately, since a REPL driven
by a background agent would stall forever behind a modal. Run the suite with the
keychain switched off instead:

```bash
LAUNCHPAD_STUDIO_NO_KEYCHAIN=1 cargo test --workspace --no-fail-fast
```

**If these four are the only failures, your change is fine.**

CI needs no such flag. `keychain_forbidden()`
(`crates/ao-engine-tools-provider-config/src/secret_vault.rs`) treats a set
`CI` variable as the same kill switch, so these four pass unmodified on any
standard runner. The full rationale for why an unattended process must never
reach the keychain is at the top of that file. One rough edge — the warning that
would have pointed you at the flag above never fires — is written up in
[KNOWN-GAPS.md](KNOWN-GAPS.md#the-keychain-failure-gives-you-no-pointer-to-its-own-workaround).

### Two tests are flaky under a full parallel run

One in `crates/ao-engine-tools-core/src/form_events.rs` and one in
`crates/ao-persistence/src/progress_log.rs`. Both pass when their crate runs
alone. **Re-run that crate on its own before assuming you broke it.**
[KNOWN-GAPS.md](KNOWN-GAPS.md#two-tests-are-flaky-under-a-full-parallel-run)
names them and records what is and is not known about why.

### No test can use a bare `sleep` of two seconds or more

The Bash tool rejects it before anything is spawned. `detect_bare_sleep`
(`crates/ao-engine-tools-io/src/bash/mod.rs`) matches a whole trimmed command of
the form `sleep N` where `N` is at least two, and returns a recoverable error
synchronously — no child process is ever created. This is deliberate, it is
stated to the model in `crates/ao-engine-tools-io/src/bash/prompt.rs`, and it
applies to your test fixtures exactly as it applies to a real call.

So if your test needs a process to still be running when it does something to
it, do not reach for `sleep`. **Use `tail -f /dev/null`.** It clears the guard
categorically rather than by finding a hole in it, blocks on I/O at roughly zero
CPU, and dies immediately on SIGTERM. Do not write `sleep 0.01 && sleep 30` or
similar: that only works because the guard declines to parse a compound command,
and it silently stops testing anything the moment the guard is tightened.

Assert elapsed-time bounds as well as the outcome. A cancellation test that
checks only "the call came back with an error" passes just as happily when no
process was ever spawned — which is precisely how the cancellation test here
spent two months red against a diagnosis that was wrong about its own cause.
`bash_bare_sleep_guard_rejects_long_sleep` and `bash_foreground_cancellation`
(`crates/ao-engine-tools-runner/tests/bash_e2e.rs`) are the worked example of
both halves.

The general technique, worth applying beyond this one guard: comment out the
line that performs the thing under test and re-run. If the test still passes, it
was never testing that.

### Use `cargo test`, not `cargo test --lib`

Some MCP tests spawn a fixture binary (`echo_mcp_server`) that they locate on
disk rather than through cargo, because cargo only exposes `CARGO_BIN_EXE_*` to
integration tests and these are unit tests asserting against module-private
state. The consequence is that `--lib` neither builds nor rebuilds that fixture:

- On a fresh clone, `cargo test --lib` as your *first* command fails ~51 tests
  that have nothing to do with your change. Running plain `cargo test` once
  fixes it permanently for that target directory.
- After editing `crates/ao-engine-tools-runner/tests/fixtures/echo_mcp_server.rs`,
  a `--lib` run silently tests the *previous* build and passes. Re-run without
  `--lib`.

The first case fails loudly with these instructions. The second cannot be
detected from inside the test — a stale binary is indistinguishable from a
current one — so it is documented here rather than checked. See
`crates/ao-engine-tools-runner/src/mcp/test_support.rs` for the full rationale.

## Conventions

**Tool modules.** A tool generally lives in its own folder with three files:

```
<crate>/src/<tool_name>/
├── mod.rs      // the struct, its trait impl, and the register helper
├── prompt.rs   // the model-facing description and input schema
└── tests.rs    // the tests
```

Keeping model-facing strings in `prompt.rs` means you can tune what the model
reads without it showing up as a behaviour diff. Thirty of the tool modules
follow this today; you will also find infrastructure modules (`context/`,
`trust_gate/`, `skill_registry/`) and a few older tools that do not. **Follow
the pattern for new tools rather than copying whatever module is nearest.**

**Comments must stand alone.** Write them for someone who has only ever seen
this repository. Do not reference internal tickets, other codebases, or
conversations. A comment that describes what the code was *meant* to do, rather
than what it does, is treated as a bug of the same severity as wrong code.

**Prefer deleting to generalising.** If a feature is half-built, removing it is
usually a better pull request than adding an abstraction over it.

## Pull requests

- Branch off `main`.
- Keep the diff focused. Large mechanical reformatting mixed with a behaviour
  change is very hard to review and will usually be sent back.
- Say what you tested. "Added a unit test" and "confirmed the live path reaches
  it" are different claims — a passing unit test proves a function works, not
  that anything calls it. If you added a feature, show that something reaches it.
- If you changed behaviour, update the docs in the same pull request. The docs
  link checker runs in CI and will fail on a broken relative link.
- If your change is not finished, open it as a draft.

### What tends to get pushed back

- Errors swallowed into a default value, a success return, or an empty result.
  If three states exist (missing, present, present-but-stale), collapsing them
  into two is a defect, not a simplification.
- New dependencies without a reason in the pull request description. Every
  dependency is a licence and a supply-chain question — see
  [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). If you add or bump one, run
  `dev/generate-third-party-notices.mjs` and commit the result; CI checks it.
- Disabling or deleting a failing test to make CI green. Say it fails and why.

## Reporting bugs and requesting features

Use the issue templates. For a bug, the reproduction steps matter more than
anything else in the report.

**Security vulnerabilities do not go in issues** — see
[SECURITY.md](SECURITY.md) for the private reporting channel.

## Licensing of your contribution

Contributions are accepted under the [Apache License 2.0](LICENSE), subject to
the additional grants in the [CLA](CLA.md).
