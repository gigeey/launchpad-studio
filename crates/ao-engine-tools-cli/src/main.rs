//! `ao-engine-tools-cli` — stdin/stdout dogfood REPL for the Anthropic provider.
//!
//! Drives `run_session` from stdin → stdout so dialect bugs and tool-description
//! issues can be reproduced without spinning up the Tauri UI. Config is loaded
//! from `<data-root>/providers.toml` (see `ao-engine-tools-provider-config`).

mod cli_render;
mod env_block;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use clap::Parser;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_core::{NoopDenialTracker, PermissionMode, Registry, RunnerContext};
use ao_engine_tools_provider_anthropic::AnthropicClient;
use ao_engine_tools_provider_config::ProviderConfig;
use ao_engine_tools_runner::{
    ContentBlock, Message,
    hooks::config::load_runner_settings,
    prompt_bridge::StdinBridge,
    query_loop::{RunnerConfig, SessionEventSink, run_session},
};

use crate::cli_render::StdoutSink;

/// Dogfood REPL that drives run_session from stdin/stdout against the Anthropic
/// provider. Used for reproducing dialect bugs without the Tauri UI.
#[derive(Parser)]
#[command(name = "ao-engine-tools-cli")]
struct Args {
    /// Provider to use. Only "anthropic" is valid in this loop; "openai" and
    /// "gemini" reserve the slot and return a clear not-implemented error.
    #[arg(long)]
    provider: String,

    /// Override the model from providers.toml.
    #[arg(long)]
    model: Option<String>,

    /// Override the built-in system prompt.
    #[arg(long)]
    system: Option<String>,

    /// Maximum number of REPL turns before the session exits automatically.
    #[arg(long, default_value = "50")]
    max_turns: usize,
}

/// Deliberately synchronous and deliberately not `#[tokio::main]`: the
/// keychain-forbidden propagation below must run before any additional
/// thread exists (see `propagate_keychain_forbidden`'s doc), and a
/// `#[tokio::main] async fn main()` fails that requirement — the runtime
/// the macro builds spawns its worker threads before the body of the
/// annotated `async fn` ever starts running, so even a call on its very
/// first line would already be too late. Building the runtime by hand
/// here, after the propagation call, keeps the ordering explicit.
fn main() {
    // Reify this process's keychain-forbidden determination into its own
    // environment before anything else runs, so every child this REPL
    // spawns for the rest of its life — current or future, from any tool
    // — inherits it automatically. Must stay the first statement in `main`:
    // see `propagate_keychain_forbidden`'s doc for why.
    ao_engine_tools_provider_config::propagate_keychain_forbidden();

    let runtime = tokio::runtime::Runtime::new().expect("failed to build the tokio runtime");
    runtime.block_on(run());
}

async fn run() {
    // This REPL has no window to prompt through even when a human is
    // driving it at a terminal — a macOS keychain "wants to use your
    // confidential information" modal popping up over stdin/stdout would
    // just stall the session, and it stalls forever outright when this
    // binary is driven non-interactively by a background agent. Only the
    // Tauri desktop app is a legitimate surface for that authorization
    // prompt.
    ao_engine_tools_provider_config::disable_interactive_keychain_prompts();

    let args = Args::parse();

    match args.provider.as_str() {
        "anthropic" => {}
        "openai" | "gemini" => {
            eprintln!(
                "error: provider {:?} is not implemented in this loop (only \"anthropic\" is valid)",
                args.provider
            );
            std::process::exit(1);
        }
        other => {
            eprintln!("error: unknown provider {:?}", other);
            std::process::exit(1);
        }
    }

    // Resolve config path early so error messages can show it.
    let config_path = match ProviderConfig::config_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve provider config path: {e}");
            std::process::exit(1);
        }
    };

    let provider_config = match ProviderConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "error: failed to load provider config from {}: {e}",
                config_path.display()
            );
            std::process::exit(1);
        }
    };

    let mut anthropic_cfg = match provider_config.anthropic {
        Some(cfg) => cfg,
        None => {
            eprintln!(
                "error: [anthropic] section missing in {}",
                config_path.display()
            );
            std::process::exit(1);
        }
    };
    if let Some(model) = &args.model {
        anthropic_cfg.model = model.clone();
    }

    let client: Arc<dyn ao_engine_tools_runner::provider::ProviderClient> =
        Arc::new(AnthropicClient::from_config(anthropic_cfg));

    // Build full tool registry once and share it across REPL turns.
    let mut registry = Registry::new();
    ao_engine_tools_runner::register_all(&mut registry);
    registry.build_deferred_index();
    let registry = Arc::new(registry);

    // Load runner settings from the process's cwd.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let settings = match load_runner_settings(&cwd) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: failed to load runner settings ({e}); using defaults");
            Default::default()
        }
    };

    // Assemble the system prompt the model sees: a `<cli-environment>`
    // block so the model knows cwd / platform / OS / shell / date /
    // git-repo status without having to ask, optionally followed by the
    // operator-supplied `--system` text. Without this block the Read /
    // Bash tools require absolute paths but the model has no source for
    // the cwd, which derails the very first turn.
    let env_block = env_block::render(&cwd);
    let system_prompt = match args.system.as_ref() {
        Some(extra) if !extra.trim().is_empty() => Some(format!("{env_block}\n\n{extra}")),
        _ => Some(env_block),
    };

    // Transcript accumulates across REPL turns so the assistant sees context.
    let mut messages: Vec<Message> = Vec::new();
    let mut turn_count = 0usize;

    // Live output sink — stdout is the user's terminal, so assistant
    // chunks, tool_use blocks, and tool_result payloads are streamed
    // through this sink as soon as the runner produces them. Kept as a
    // single instance across REPL turns so the in-text-run state cleanly
    // resets between turns via `finish_turn()`.
    let stdout_sink: Arc<StdoutSink> = Arc::new(StdoutSink::new());

    // Initialise the line editor up front so any tty-init failure surfaces
    // before the first prompt. Held in an `Option` across iterations because
    // the editor must be moved into `spawn_blocking` (rustyline's read loop
    // is blocking, and we need to free the runtime thread while it sits on
    // the keyboard) and handed back via the join result so in-memory history
    // accumulates across turns.
    let mut editor: Option<DefaultEditor> = match DefaultEditor::new() {
        Ok(ed) => Some(ed),
        Err(e) => {
            eprintln!("error: failed to initialise line editor: {e}");
            std::process::exit(1);
        }
    };

    // === Long-lived Ctrl-C handler ========================================
    //
    // A naive per-turn `tokio::signal::ctrl_c()` task has two failure modes:
    //   1. The first call to `ctrl_c()` installs a process-wide SIGINT
    //      handler that overrides the OS default (kill the process). When
    //      the per-turn task is aborted between turns, no task is awaiting
    //      the signal stream, so subsequent Ctrl-C presses are silently
    //      swallowed — including at the `> ` prompt.
    //   2. Even mid-turn, a single Ctrl-C only cancels the current turn
    //      with no path to exit the process from the keyboard short of
    //      Ctrl-D.
    //
    // Fix: install one signal task at startup that lives for the entire
    // process. It treats the first interrupt during a turn as "cancel the
    // turn" and any subsequent interrupt — or an interrupt at the prompt
    // with no active turn — as "hard exit with code 130" (128 + SIGINT,
    // the convention used by rustc / cargo / most shells).
    //
    // The slot's `Option<CancellationToken>`:
    //   * `Some(cancel)` while a turn is in flight; `cancel.is_cancelled()`
    //     also doubles as the "already interrupted once" flag for this turn.
    //   * `None` between turns (sitting at the `> ` prompt). Any signal
    //     that fires here goes straight to exit(130).
    let active_turn_cancel: Arc<StdMutex<Option<CancellationToken>>> =
        Arc::new(StdMutex::new(None));
    let active_for_signal = active_turn_cancel.clone();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            let snapshot = active_for_signal
                .lock()
                .ok()
                .and_then(|guard| guard.clone());
            match snapshot {
                Some(cancel) if !cancel.is_cancelled() => {
                    // First interrupt this turn — cancel the in-flight
                    // session. The REPL will print `<cancelled>` when
                    // run_session returns and loop back to the prompt.
                    eprintln!(
                        "\n(interrupt — press Ctrl-C again to exit, or Ctrl-D for clean exit)"
                    );
                    cancel.cancel();
                }
                _ => {
                    // No active turn (slot=None at the prompt) or this
                    // turn's cancel was already fired — second strike.
                    std::process::exit(130);
                }
            }
        }
    });

    loop {
        if turn_count >= args.max_turns {
            eprintln!("max-turns ({}) reached", args.max_turns);
            break;
        }

        // Drive readline on a blocking thread so it can own the keyboard
        // (raw mode in a tty, plain `read_line` in a non-tty test pipe)
        // without parking the tokio runtime. The editor is moved in and
        // handed back through the task return so subsequent iterations
        // see the same in-memory history buffer.
        let mut ed = editor.take().expect("editor present at top of loop");
        let join = tokio::task::spawn_blocking(move || {
            let result = ed.readline("> ");
            (result, ed)
        })
        .await;
        let (line_result, returned_editor) = match join {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("readline task panicked: {e}");
                break;
            }
        };
        editor = Some(returned_editor);

        let line = match line_result {
            Ok(s) => s,
            Err(ReadlineError::Eof) => break, // Ctrl-D
            // Ctrl-C in rustyline means "discard current line, redraw
            // prompt". The long-lived signal task above already handles
            // exit-on-second-Ctrl-C; the convention bash/zsh/python use
            // is "Ctrl-C clears the line and stays at the prompt", which
            // matches what `continue` does here. The signal task's
            // exit(130) path still wins the race when the slot is None
            // (no active turn), so a single Ctrl-C at an idle prompt still
            // exits.
            Err(ReadlineError::Interrupted) => continue,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        };

        let line = line.trim_end_matches('\r').to_string();
        if line.is_empty() {
            continue;
        }

        // Push the typed line into history so ↑ recalls it. Errors here
        // (rare — only fail under OOM) are non-fatal: the turn still runs.
        if let Some(ed) = editor.as_mut() {
            let _ = ed.add_history_entry(line.as_str());
        }

        messages.push(Message::User {
            content: vec![ContentBlock::Text { text: line }],
        });
        let before_len = messages.len();

        // Fresh RunnerContext per turn — gives us a fresh CancellationToken
        // for this turn's ctrl-C handler without affecting subsequent turns.
        let ctx =
            RunnerContext::new_with_cwd(uuid::Uuid::new_v4().to_string(), "ao-engine-tools-cli", cwd.clone())
                .with_registry(registry.clone());

        let cancel = ctx.cancel.clone();

        // Hand this turn's cancel token to the long-lived signal task. The
        // `is_cancelled()` flag on the token is what distinguishes a first
        // Ctrl-C ("cancel the turn") from a second one ("exit the process").
        if let Ok(mut slot) = active_turn_cancel.lock() {
            *slot = Some(cancel.clone());
        }

        let config = RunnerConfig {
            provider: client.clone(),
            // Interactive REPL — the human at the terminal IS the operator,
            // so prompts route to stdin/stdout via StdinBridge instead of
            // the fail-closed StubBridge that ships with SDK embeddings.
            bridge: Arc::new(StdinBridge::new()),
            denial_tracker: Arc::new(NoopDenialTracker),
            settings: settings.clone(),
            mode: PermissionMode::Default,
            kind: ao_engine_tools_core::SessionKind::Interactive,
            auto_approve: vec![],
            system_prompt: system_prompt.clone(),
            // Stream assistant chunks / tool_use / tool_result to stdout
            // as the runner produces them; this is what makes Ctrl-C
            // mid-stream show partial output and surfaces tool_result
            // lines in the dogfood loop.
            event_sink: Some(stdout_sink.clone() as Arc<dyn SessionEventSink>),
            // CLI dogfood loop has no agent profile to read a thinking
            // config from — leave it unset so the API path doesn't
            // unexpectedly enable extended thinking from a terminal
            // session. A future flag (`--thinking summarized`) can
            // override this when the dev wants to exercise the path.
            thinking: None,
            max_turns: None,
        };

        let outcome = run_session(messages.clone(), ctx, config).await;
        // Flush any half-written assistant text line so the next prompt
        // does not paste onto the tail of the previous chunk.
        stdout_sink.finish_turn();

        // Clear the slot so a Ctrl-C at the upcoming `> ` prompt exits the
        // process instead of firing a stale (or already-cancelled) token.
        if let Ok(mut slot) = active_turn_cancel.lock() {
            *slot = None;
        }

        match outcome {
            Ok(session_out) => {
                if session_out.cancelled || cancel.is_cancelled() {
                    println!("<cancelled>");
                    io::stdout().flush().ok();
                }
                messages = session_out.messages;
            }
            Err(e) => {
                eprintln!("[error] {e}");
                // Remove the user message we pushed — the turn failed entirely.
                messages.pop();
            }
        }
        // `before_len` is unused now that the sink emits everything
        // live; keep the binding silenced rather than restructuring the
        // turn loop around it.
        let _ = before_len;

        turn_count += 1;
    }
}
