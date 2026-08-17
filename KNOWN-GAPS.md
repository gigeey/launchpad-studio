# Known gaps

Everything here is known, reproducible, and deliberately not fixed yet. Each entry says what
it is, where it is, what it costs you in practice, and why it was left. Nothing in this file is
a surprise waiting for you — that is the whole point of the file existing.

If you hit something that is *not* listed here, it is a bug and worth an issue.

---

## Tests

### Two tests are flaky under a full parallel run

- `form_events::tests::form_request_round_trips_event_type_and_metadata`
  (`crates/ao-engine-tools-core/src/form_events.rs`)
- `progress_log::progress_log_tests::concurrent_appends_produce_no_torn_writes`
  (`crates/ao-persistence/src/progress_log.rs`)

Both pass reliably when their own crate is run alone and have failed occasionally when the whole
workspace runs at once. **Re-run the affected crate on its own before assuming your change caused
it.** The first fails as `entries.len()` being `0` rather than `1` — that is, a
`TranscriptStore::append` that returned `Ok(())` was not yet readable.

A suspected cause has been addressed: every JSONL append site in `ao-persistence` now flushes
before dropping the handle (`crates/ao-persistence/src/transcript.rs`). Previously `write_all`
on a tokio `File` returned once the write was queued, and `Drop` discarded any error the close
reported — so a full disk could surface as `Ok(())` and a short file. That was a real gap and
worth closing on its own terms, but it is **not proven** to be the cause of these two flakes: a
targeted 200-way concurrent reproduction never triggered them, and a single green workspace run is
not evidence of a fix. They are listed here because they may still recur.

### The keychain failure gives you no pointer to its own workaround

On macOS, four tests in `crates/ao-engine-tools-cli/tests/cli_smoke.rs` fail on a developer Mac
unless you run with `LAUNCHPAD_STUDIO_NO_KEYCHAIN=1` — see
[CONTRIBUTING.md](CONTRIBUTING.md#running-the-tests) for the full explanation and the command.

The rough edge, documented rather than fixed because the fix belongs with the next change to that
module: the code has a `tracing::warn!` that names both ways out of precisely this situation, but
it is gated on the OSStatus for "interaction not allowed" (two call sites in `secret_vault.rs`),
and this failure reports an authentication failure instead. **The hint never fires.** You get the raw
keychain error and no pointer to the flag.

---

## Engine

### Co-pilot enrolment is only partly ownership-aware

The tasklist co-pilot itself works for both ownership kinds: `TasklistStore::find_by_copilot_agent_id`
(`crates/ao-persistence/src/tasklist_store.rs`) walks both `teams/<team_id>/tasklists/` and
`tasks/agents/<agent_id>/tasklists/`, so context injection and the
`<tasklist action="append">` tag both resolve a project tasklist's binding.

What is *not* ownership-aware is the mailbox poller's enrolment plumbing
(`crates/ao-engine/src/mailbox_poller.rs`). The `TasklistWoke` / `TasklistSlept` lifecycle
events carry a bare `team_id` and no owner, so:

- the poller's **startup rebuild** enumerates `teams/` only, and
- its **wake reactor** resolves that team id with a team-keyed `get`.

Neither can ever match an agent-owned tasklist. In practice this is covered rather than broken:
project co-pilots enrol on demand via wake-on-deliver in
`QueueManagerRegistry::submit_message` (`crates/ao-engine/src/queue_manager.rs`), which keys off
the agent's profile template rather than tasklist ownership, and the sleep sweep evicts them
inline for the same reason. **The residual gap is that a project co-pilot is not pre-enrolled at
process start — it enrols on its first delivered message instead.**

Closing it properly means carrying the owner on the lifecycle events instead of a bare team id.
That is a protocol change touching the SSE stream, so it was left out deliberately rather than
worked around.

---

## Providers

### The Gemini provider is not reachable

**Where:** `crates/ao-engine-tools-provider-gemini/` (2,962 lines across 9 files), and the
`[gemini]` section of [`providers.toml.example`](providers.toml.example).

A Gemini API key is accepted, validated against the known-provider list, and moved into your OS
keychain like any other. **No agent can then be pointed at it.** `NativeProvider`
(`crates/ao-protocol/src/agent.rs`) has exactly three variants — `Anthropic`, `Openai`,
`OpenRouter` — and `DefaultProviderFactory::build` (`crates/ao-engine/src/agent_runner/native.rs`)
matches on that enum, so there is no value a profile could carry that would select the Gemini
client. Every public type the crate exports has zero referents outside it.

The cost is that "configured and working" and "configured and silently ignored" look identical
from the outside: you set the key, nothing errors, and nothing uses it.

It compiles without a warning because `dead_code` only considers reachability *within* a crate,
and these are `pub` items in a library crate — externally reachable by definition, so rustc
cannot flag them however few callers exist.

Two honest ways out, and this is an owner decision rather than a mechanical one: **wire it up**
(a `NativeProvider::Gemini` variant plus a factory arm, then an end-to-end assertion that a
Gemini-mode agent actually completes a turn), or **delete the crate and the `[gemini]` config
section** so the key is rejected instead of quietly stored. Either direction is a product call,
so the gap is recorded here rather than closed unilaterally.

Note the Google **Antigravity** CLI (`agy`) is a separate thing entirely and does work — that is
CLI mode driving an installed binary, and it does not touch this crate.

---

## Build and tooling

### Neither `cargo fmt --check` nor `cargo clippy` gates a merge

Both fail on the tree as it stands: 548 of 760 Rust source files differ from rustfmt's output, and
clippy's deny-by-default `approx_constant` fires on a JSON literal in a test fixture.

Enabling either today would mean a permanently red badge or a ~550-file mechanical reformat, and a
diff that size immediately before a public release buries far more than it reveals. Both are worth
turning on. Neither was worth turning on that week. The reasoning is also recorded at the top of
`.github/workflows/ci.yml`, next to the decision it explains.

---

## Platform

### Windows and Linux builds are unverified, and the updater is macOS-only

CI builds and tests the application on macOS only, and the published update manifest lists
`darwin-aarch64` and `darwin-x86_64` and nothing else — so on Linux and Windows every updater
check fails with `TargetsNotFound` whether or not a newer version exists. The full statement of
what is and is not verified per platform is in the
[README](README.md#platform-support-and-known-limitations).
