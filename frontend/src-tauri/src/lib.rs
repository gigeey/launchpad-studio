mod plugin_commands;

use std::sync::Arc;
use std::time::Duration;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Where very-early startup diagnostics (AppState-init failures, the
/// fallback decision, and any panic before the rest of the logging stack is
/// up) are appended — see [`log_startup_diagnostic`] and
/// [`install_startup_panic_hook`].
///
/// Deliberately NOT under the resolved data root: an unusable data root is
/// exactly the failure this file needs to survive describing, so it goes to
/// the OS temp directory instead, which doesn't depend on the data-root
/// resolver (or the user's home directory) at all. A plain append-only text
/// file — not JSON, not routed through `tracing` — so it stays readable
/// with `cat`/`tail` even if nothing else in the app ever finishes
/// starting.
fn startup_diagnostics_log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("launchpad_studio_startup.log")
}

/// Append one line to the persistent startup-diagnostics log
/// ([`startup_diagnostics_log_path`]), stamped with a Unix-epoch-seconds
/// timestamp (no date/time-formatting dependency is worth pulling in for a
/// log whose entire job is to still be readable when things are already
/// going wrong). Best-effort: a failure to open or write the file is itself
/// printed to stderr rather than silently dropped, but is never escalated
/// into a second panic — this function must not be a new way for startup to
/// fail.
fn append_startup_diagnostic(message: &str) {
    use std::io::Write;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{timestamp}] {message}\n");
    let path = startup_diagnostics_log_path();
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => {
            if let Err(e) = file.write_all(line.as_bytes()) {
                eprintln!("[startup-log] failed to write {}: {e}", path.display());
            }
        }
        Err(e) => {
            eprintln!("[startup-log] failed to open {}: {e}", path.display());
        }
    }
}

/// Print `message` to stderr AND append it to the persistent startup log.
/// Use this at explicit startup error/fallback call sites — as opposed to
/// [`install_startup_panic_hook`]'s hook, which only appends to the file
/// and lets the default panic hook keep owning stderr's panic formatting.
fn log_startup_diagnostic(message: &str) {
    eprintln!("{message}");
    append_startup_diagnostic(message);
}

/// Install a panic hook that appends every panic's message and location to
/// the persistent startup log before falling through to the default hook
/// (which still prints to stderr as usual). Must be the first thing `run()`
/// does, so it's in place for the whole startup sequence — including the
/// final `.expect("error while running tauri application")` on the
/// Tauri builder's `.run()` call, which is otherwise a bare panic with no
/// durable record once the terminal that launched this process is gone.
///
/// This does NOT reach the panic this whole mechanism was originally built
/// to make diagnosable: a panic inside tao's `did_finish_launching`
/// (invoked from deep inside `.run()`, across an ObjC/nounwind FFI
/// boundary) is converted to `abort()` before Rust's panic machinery — this
/// hook included — ever sees it. Every panic on our own side of that
/// boundary, though, now leaves a trail here instead of only ever having
/// existed in a terminal buffer nobody was watching.
fn install_startup_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        append_startup_diagnostic(&format!("[panic] {info}"));
        default_hook(info);
    }));
}

/// Write PNG image bytes to the system clipboard (macOS only).
#[tauri::command]
fn copy_image_to_clipboard(png_data: Vec<u8>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use objc::runtime::Object;
        use objc::{class, msg_send, sel, sel_impl};

        unsafe {
            let pasteboard: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
            let _: () = msg_send![pasteboard, clearContents];

            let ns_data: *mut Object = msg_send![class!(NSData),
                dataWithBytes: png_data.as_ptr()
                length: png_data.len()
            ];

            // UTType for PNG: "public.png"
            let png_type: *mut Object =
                msg_send![class!(NSString), stringWithUTF8String: b"public.png\0".as_ptr()];

            let success: bool = msg_send![pasteboard, setData: ns_data forType: png_type];
            if !success {
                return Err("NSPasteboard setData:forType: returned false".into());
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = png_data;
        Err("Clipboard image copy is only supported on macOS".into())
    }
}

/// Opens a native folder picker and returns the selected path, or `None` if
/// the user cancels.
///
/// Deliberately does NOT go through the dialog plugin's own `open`/`pick_folder`
/// commands. Two reasons:
///
/// 1. Those commands resolve a parent window automatically (`NSApp.mainWindow()`)
///    and show the panel as a sheet attached to it. On this app's main window
///    that attachment is unreliable — the vibrancy setup in `run()`'s `.setup()`
///    hook makes the window transparent and layers an `NSVisualEffectView` into
///    its view hierarchy, which can prevent the sheet's completion handler from
///    ever firing and previously caused an unrecoverable `RecvError` panic.
///    Calling `rfd`'s synchronous `pick_folder()` directly shows a plain
///    application-modal panel with no sheet/parent-window involved, avoiding
///    that interaction entirely.
/// 2. It lets us wrap the panel construction in `catch_unwind`. macOS's
///    `NSOpenPanel::openPanel` panics (rather than returning an error) when the
///    OS-hosted `com.apple.view-bridge` panel service refuses the request —
///    e.g. when the running process's on-disk binary no longer matches what's
///    mapped in memory after a dev rebuild, or the process is flagged as
///    debugged/instrumented. Going through the dialog plugin's own async
///    `pick_folder` wrapper, that panic fires inside a closure the plugin
///    schedules onto the main thread internally, a call stack this command
///    can't reach with `catch_unwind`. Building the panel ourselves inside our
///    own `run_on_main_thread` closure keeps the whole chain in one frame we
///    control, so a refused panel becomes a `None` result instead of taking
///    down the whole app.
#[tauri::command]
async fn pick_directory(app: tauri::AppHandle) -> Option<String> {
    use std::panic::AssertUnwindSafe;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let dispatched = app.run_on_main_thread(move || {
        let picked = std::panic::catch_unwind(AssertUnwindSafe(|| rfd::FileDialog::new().pick_folder()))
            .unwrap_or(None);
        let _ = tx.send(picked);
    });
    if dispatched.is_err() {
        return None;
    }

    rx.await
        .ok()
        .flatten()
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
fn open_devtools(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "No main window found".to_string())?;
    window.open_devtools();
    Ok(())
}

#[tauri::command]
fn set_vibrancy(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri::Manager;
        let window = app
            .get_webview_window("main")
            .ok_or_else(|| "No main window found".to_string())?;
        if enabled {
            window_vibrancy::apply_vibrancy(
                &window,
                window_vibrancy::NSVisualEffectMaterial::UnderPageBackground,
                None,
                None,
            )
            .map_err(|e| format!("Failed to apply vibrancy: {:?}", e))?;

            // Make WKWebView transparent so vibrancy shows through
            let _ = window.with_webview(|webview| {
                use objc::runtime::{Object, NO};
                use objc::msg_send;
                use objc::sel;
                use objc::sel_impl;
                use objc::class;

                unsafe {
                    let wk_webview: *mut Object = webview.inner().cast();
                    let key: *mut Object = msg_send![class!(NSString), stringWithUTF8String: b"drawsBackground\0".as_ptr()];
                    let no_val: *mut Object = msg_send![class!(NSNumber), numberWithBool: NO];
                    let _: () = msg_send![wk_webview, setValue: no_val forKey: key];
                }
            });

            eprintln!("[vibrancy] Applied vibrancy + transparent webview");
        } else {
            // Restore opaque webview background
            let _ = window.with_webview(|webview| {
                use objc::runtime::{Object, YES};
                use objc::msg_send;
                use objc::sel;
                use objc::sel_impl;
                use objc::class;

                unsafe {
                    let wk_webview: *mut Object = webview.inner().cast();
                    let key: *mut Object = msg_send![class!(NSString), stringWithUTF8String: b"drawsBackground\0".as_ptr()];
                    let yes_val: *mut Object = msg_send![class!(NSNumber), numberWithBool: YES];
                    let _: () = msg_send![wk_webview, setValue: yes_val forKey: key];
                }
            });

            window_vibrancy::clear_vibrancy(&window)
                .map_err(|e| format!("Failed to clear vibrancy: {:?}", e))?;
            eprintln!("[vibrancy] Cleared vibrancy + restored opaque webview");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, enabled);
    }
    Ok(())
}

#[tauri::command]
async fn check_cli_available(command: String, version_flag: String) -> Result<bool, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = std::process::Command::new(&command)
            .arg(&version_flag)
            .env("PATH", ao_process::shell_path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = tx.send(result);
    });
    rx.recv().map_err(|e| e.to_string())
}

/// What [`restart_app`] actually did, reported back to the frontend so it
/// can tell a real restart apart from the dev build's deliberate no-op —
/// see that command's doc comment. A bare success/failure isn't enough
/// here: both outcomes below resolve the invoking JS promise normally, but
/// only one of them means the process is actually going down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RestartOutcome {
    /// The process is restarting itself right now; the caller has nothing
    /// further to do.
    Restarting,
    /// Running under `cfg!(debug_assertions)`. Nothing was restarted — the
    /// frontend must tell whoever is running the dev server to restart it
    /// by hand for the workspace switch to take effect.
    DevRestartRequired,
}

/// Restarts the whole application process.
///
/// The packaged app runs its backend embedded inside this same process (see
/// `start_backend_server` below) rather than as a standalone server the OS
/// could bounce on its own — `AppState::new()` is built once here and
/// shared with the axum router via `Manager::manage`. So "switch active
/// workspace" has no lighter-weight implementation than restarting the
/// whole process: there's nothing narrower to restart.
///
/// The relaunched process picks up the newly-active workspace by reading
/// the on-disk registry (`~/.launchpad_studio/workspaces.json`) at startup
/// — never from an inherited env var, since a restarted process inherits
/// this one's environment unchanged. This command only triggers the
/// restart; writing the new `active` pointer is `POST
/// /workspaces/{id}/activate`'s job (the HTTP layer), and must already have
/// completed before the frontend calls this.
///
/// That inheritance is exactly why this clears a startup-fallback pin
/// first. When `start_backend_server` recovers from an unusable data root
/// it pins `LAUNCHPAD_STUDIO_DATA_DIR` on this process, and Tauri's restart
/// re-launches the current binary via `std::process::Command` without
/// clearing the environment — so that in-process `setenv` survives into the
/// relaunched process, where nothing records a fallback anymore and it
/// therefore reads as a deliberate operator pin. Left in place it would
/// outrank the registry forever, silently discarding the very activation
/// this restart exists to apply. [`ao_protocol::data_root::unpin_startup_fallback_root`]
/// clears ONLY that self-inflicted pin; a pin inherited from the launching
/// environment (the dual-worktree contract's) is left untouched, so a
/// restart there still comes back on its own track.
///
/// Uses `request_restart()` rather than `AppHandle::restart()`: the latter
/// blocks its calling thread forever if not called from Tauri's main
/// thread, which a `#[tauri::command]` handler has no guarantee of running
/// on. `request_restart()` instead posts a restart-then-exit request onto
/// the event loop and returns immediately, so the invoking JS promise
/// resolves normally.
///
/// Under `cfg!(debug_assertions)` (`npm run tauri dev`) this skips the real
/// restart entirely and reports [`RestartOutcome::DevRestartRequired`]
/// instead. The tauri-cli watcher that owns the dev Vite server tears it
/// down the moment this process exits to re-exec; the relaunched binary
/// then loads its baked-in dev URL and finds nothing listening there —
/// restarting for real in dev doesn't reproduce the release behaviour, it
/// reproduces a different, broken one. Reporting the outcome instead lets
/// the frontend tell the developer to restart the dev server by hand. The
/// release path is untouched: `cfg!(debug_assertions)` is `false` there, so
/// this always falls through to the real `request_restart()` call exactly
/// as before.
#[tauri::command]
fn restart_app(app: tauri::AppHandle) -> RestartOutcome {
    if cfg!(debug_assertions) {
        // Deliberately BEFORE any unpin: nothing is relaunching here, so
        // clearing the pin would only strand this still-running process's
        // future data-root resolutions somewhere its `AppState` isn't.
        return RestartOutcome::DevRestartRequired;
    }
    if ao_protocol::data_root::unpin_startup_fallback_root() {
        log_startup_diagnostic(
            "[ao-backend] Cleared this process's startup-fallback data-root pin before \
             restarting, so the relaunched process resolves from the workspace registry.",
        );
    }
    app.request_restart();
    RestartOutcome::Restarting
}

/// What the backend init thread hands to the main thread once `AppState`
/// construction has completed — either normally, against whichever data
/// root [`ao_protocol::data_root::resolve_data_root_with_provenance`]
/// selected, or via a fallback to the default root after that root failed
/// to initialize (see [`start_backend_server`]). Carrying the `Arc` plus
/// optional fallback diagnostics — rather than a bare `Arc`, as before —
/// lets the main thread tell the two apart instead of a successful fallback
/// being indistinguishable from a normal start once construction has
/// already finished either way.
struct BackendStartup {
    state: Arc<ao_engine::AppState>,
    fallback: Option<ao_protocol::data_root::StartupFallback>,
}

/// Starts the ao-server backend on a background tokio task.
/// Mirrors the logic from `crates/ao-server/src/main.rs`.
///
/// Initializes `AppState` and sends it — wrapped in a [`BackendStartup`]
/// alongside any fallback diagnostics — over `state_tx` so the Tauri
/// command layer (which lives in the main thread) can register the same
/// instance via `tauri::Manager::manage`. Tauri's plugin commands and the
/// axum HTTP routes thus share a single `AppState` (and a single
/// `PluginCache`) — mutations from either side are visible to the other.
///
/// If `AppState::new()` fails against the normally-resolved root, this does
/// NOT panic: it retries once against
/// [`ao_protocol::data_root::default_data_root`] and, if that succeeds,
/// continues in degraded "fallback mode" rather than taking the whole app
/// down over one unusable data root. The retry works by setting
/// `LAUNCHPAD_STUDIO_DATA_DIR` to the default root for the rest of this
/// process's life (rather than passing an explicit root to a parallel
/// `AppState` constructor) so every one of the roughly twenty data-root call
/// sites across the workspace — including ones inside `AppState::new()`
/// itself that resolve the root independently of its own return value —
/// agree on the same fallback root, not just the top-level `PersistenceLayer`
/// this function can see directly. Only if BOTH attempts fail does this
/// function return without ever calling `state_tx.send(...)` — no panic, it
/// just returns — which drops `state_tx`. `run()`'s `state_rx.recv()` on
/// the main thread turns that drop into a diagnosable fatal error rather
/// than a bare panic; see that call site.
async fn start_backend_server(state_tx: std::sync::mpsc::SyncSender<BackendStartup>) {
    let primary_root = ao_protocol::data_root::resolve_data_root_or_cwd();

    let startup = match ao_engine::AppState::new().await {
        Ok(state) => BackendStartup {
            state: Arc::new(state),
            fallback: None,
        },
        Err(primary_err) => {
            log_startup_diagnostic(&format!(
                "[ao-backend] AppState init failed against {}: {primary_err}. Retrying \
                 against the default data root.",
                primary_root.display(),
            ));

            let fallback_root = match ao_protocol::data_root::default_data_root() {
                Ok(root) => root,
                Err(e) => {
                    log_startup_diagnostic(&format!(
                        "[ao-backend] FATAL: cannot determine a default data root to fall \
                         back to after {} failed to initialize ({primary_err}): {e}",
                        primary_root.display(),
                    ));
                    return;
                }
            };

            // Compute the diagnostics BEFORE pinning, so the pin and the
            // record of why it exists land together — see
            // `pin_startup_fallback_root`, which writes the record first and
            // the env var second precisely so no observer can ever see a pin
            // it can't attribute. Splitting these two around the `await`
            // below (as this used to) left every provenance check in that
            // window reporting a self-inflicted pin as a deliberate one.
            let fallback = ao_protocol::data_root::startup_fallback_diagnostics(
                &primary_root,
                Some(&primary_err.to_string()),
                &fallback_root,
            );
            if let Some(fallback) = &fallback {
                // See this function's doc comment for why the pin is a
                // global env override rather than an explicit constructor
                // parameter. `fallback` is always `Some` here — this is the
                // primary-root-failed path, and that's the only input
                // `startup_fallback_diagnostics` reports a fallback for.
                ao_protocol::data_root::pin_startup_fallback_root(fallback);
            }

            match ao_engine::AppState::new().await {
                Ok(state) => {
                    log_startup_diagnostic(&format!(
                        "[ao-backend] Started in FALLBACK mode against {} (original root {} \
                         failed to initialize)",
                        fallback_root.display(),
                        primary_root.display(),
                    ));
                    BackendStartup {
                        state: Arc::new(state),
                        fallback,
                    }
                }
                Err(fallback_err) => {
                    log_startup_diagnostic(&format!(
                        "[ao-backend] FATAL: AppState init also failed against the default \
                         data root {} ({fallback_err}). Original failure against {}: \
                         {primary_err}",
                        fallback_root.display(),
                        primary_root.display(),
                    ));
                    return;
                }
            }
        }
    };

    // Record the fallback state (if any) so `GET /workspaces/active` can
    // report it to the frontend — mirrors
    // `ao_protocol::data_root::WORKSPACE_ROOT_CACHE`'s process-wide-static
    // pattern for the resolved-root/provenance half of this same question,
    // rather than threading a new field through every `AppState`
    // constructor.
    //
    // This is the NORMAL path's recording ("no fallback happened"). The
    // fallback path above has already recorded its own, necessarily earlier
    // — the record has to be in place before the pin it explains — and this
    // call is a deliberate no-op there: the underlying record is write-once
    // and ignores a second write.
    ao_protocol::data_root::record_startup_fallback(startup.fallback.clone());

    let state = Arc::clone(&startup.state);

    // Captured here — rather than re-derived below — because
    // `state_tx.send(startup)` immediately below moves `startup` away, and
    // the startup diagnostic near the bind (further down this function)
    // needs to know which root actually ended up in use without a second
    // resolver call.
    let resolved_data_root = startup
        .fallback
        .as_ref()
        .map(|fallback| fallback.fallback_root.clone())
        .unwrap_or_else(|| primary_root.clone());
    let took_fallback_root = startup.fallback.is_some();

    // Hand the startup result to whoever is waiting on the main thread
    // before we go on to long-lived work; the receiver may have already
    // given up if Tauri shut down during init, so a send error is logged
    // but not fatal — this thread still finishes bringing the server up.
    if let Err(err) = state_tx.send(startup) {
        tracing::warn!("backend AppState handoff to Tauri failed: {err}");
    }

    // Spawn background cleanup task
    {
        let persistence = Arc::clone(&state.persistence);
        tokio::spawn(async move {
            let cleanup_interval = Duration::from_secs(30 * 60);
            let older_than = Duration::from_secs(3600);

            loop {
                match persistence.assets.cleanup_all_uncommitted(older_than).await {
                    Ok(results) => {
                        for (agent_id, count, freed) in &results {
                            tracing::info!(
                                "Cleaned up {} orphaned files for agent {}, freed {} bytes",
                                count,
                                agent_id,
                                freed
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Asset cleanup failed: {}", e);
                    }
                }
                tokio::time::sleep(cleanup_interval).await;
            }
        });
    }

    // Read port from env
    let port: u16 = std::env::var("AO_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);

    // Unconditional startup diagnostic — emitted before the AO_EMBEDDED_SERVER
    // opt-out check below so it prints in every mode, including when the
    // embedded server is skipped entirely. The actual bug this prevents is
    // AO_PORT and VITE_API_BASE_URL silently diverging with nothing printed
    // about which port or data root either side chose.
    let embedded_server_disabled = std::env::var("AO_EMBEDDED_SERVER").as_deref() == Ok("0");
    log_startup_diagnostic(&format!(
        "[ao-backend] port={port} data_root={}{} embedded_server={}",
        resolved_data_root.display(),
        if took_fallback_root { " (fallback root)" } else { "" },
        if embedded_server_disabled { "skipped" } else { "starting" },
    ));

    // SAFETY-OF-RETURN: the AppState handoff via `state_tx.send` above has already
    // completed, so the main thread is no longer blocked on `state_rx.recv()`.
    // Returning here is only safe *after* that send — never move this earlier.
    if embedded_server_disabled {
        log_startup_diagnostic(&format!(
            "[ao-backend] AO_EMBEDDED_SERVER=0 — skipping embedded server; expecting an external ao-server on port {port}"
        ));
        return;
    }

    let router = ao_server::routes::build_router(Arc::clone(&state));

    let listener = match tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port)).await {
        Ok(l) => l,
        Err(e) => {
            log_startup_diagnostic(&format!(
                "[ao-backend] Could not bind 127.0.0.1:{port} ({e}); not starting embedded server. \
                 Assuming an external ao-server already owns this port — the frontend will attach to it."
            ));
            return;
        }
    };

    tracing::info!("ao-server listening on 127.0.0.1:{}", port);
    if let Err(e) = axum::serve(listener, router).await {
        log_startup_diagnostic(&format!("[ao-backend] Server error: {e}"));
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Must be the very first thing `run()` does — see this function's doc
    // comment for what it does and doesn't cover.
    install_startup_panic_hook();

    // Initialize tracing before anything else so it captures all crates.
    // logger: for now we are checking ao_server, ao_engine, and ao_process can remove later
    ao_server::log_buffer::init();

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("ao_server=debug,ao_engine=debug,ao_process=debug,ao_normalizer=trace"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(ao_server::log_buffer::BufferLayer)
        .init();

    // Spawn the backend server on a background thread with its own tokio runtime,
    // so it doesn't conflict with Tauri's runtime.
    // Use a named thread and catch panics so server failures are visible in logs
    // instead of silently dying.
    //
    // The bounded channel hands a `BackendStartup` (the freshly-built
    // `Arc<AppState>` plus optional fallback diagnostics — see that type's
    // doc comment) from the backend thread back to the main thread so
    // Tauri commands can register the same `AppState` instance via
    // `.manage()`.
    let (state_tx, state_rx) = std::sync::mpsc::sync_channel::<BackendStartup>(1);
    std::thread::Builder::new()
        .name("ao-backend".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(move || {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to build tokio runtime for backend")
                    .block_on(start_backend_server(state_tx));
            });
            if let Err(panic_info) = result {
                let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                log_startup_diagnostic(&format!("[ao-backend] Server thread panicked: {}", msg));
            }
        })
        .expect("Failed to spawn backend thread");

    // Block until the backend has finished initializing AppState. This adds
    // ~1-2s to perceived startup, but the alternative (Tauri commands
    // returning "backend not ready" until init completes) is worse UX since
    // the frontend's HTTP traffic also can't succeed until the same moment.
    //
    // A dropped sender here (`recv()` returning `Err`) means the backend
    // thread exited without ever completing `start_backend_server` — i.e.
    // `AppState::new()` failed against BOTH the resolved root and the
    // default-root fallback (see that function), or the thread panicked
    // before reaching either attempt. Either way there is no data root this
    // process can run against, so exiting is the only option left — but
    // unlike a bare `.expect()`, the reason is written somewhere that
    // outlives this terminal before we do.
    let startup = match state_rx.recv() {
        Ok(startup) => startup,
        Err(_) => {
            log_startup_diagnostic(
                "[ao-backend] FATAL: backend thread exited before initializing AppState under \
                 any data root (resolved or default). See the preceding [ao-backend] log \
                 lines above (also written to this same file) for the underlying error(s). \
                 Launchpad Studio cannot start.",
            );
            log_startup_diagnostic(&format!(
                "[ao-backend] Full startup diagnostics: {}",
                startup_diagnostics_log_path().display(),
            ));
            std::process::exit(1);
        }
    };

    if let Some(fallback) = &startup.fallback {
        tracing::warn!(
            failed_root = %fallback.failed_root.display(),
            fallback_root = %fallback.fallback_root.display(),
            error = %fallback.error,
            "Launchpad Studio started in FALLBACK mode: the configured data root failed to \
             initialize, so the default data root is in use instead for this run. See \
             GET /workspaces/active for the same detail exposed to the frontend.",
        );
    }
    let app_state = startup.state;

    tauri::Builder::default()
        .manage(app_state)
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // Apply vibrancy early so it's ready before the webview content renders.
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;
                if let Some(window) = app.get_webview_window("main") {
                    // 1. Apply native vibrancy to the NSWindow
                    let _ = window_vibrancy::apply_vibrancy(
                        &window,
                        window_vibrancy::NSVisualEffectMaterial::UnderPageBackground,
                        None,
                        None,
                    );
                    eprintln!("[vibrancy] Applied UnderPageBackground vibrancy at startup");

                    // 2. Make the WKWebView itself transparent so vibrancy shows through
                    let _ = window.with_webview(|webview| {
                        use objc::runtime::{Object, NO};
                        use objc::msg_send;
                        use objc::sel;
                        use objc::sel_impl;
                        use objc::class;

                        unsafe {
                            let wk_webview: *mut Object = webview.inner().cast();

                            // WKWebView.setValue(false, forKey: "drawsBackground")
                            // This is the most reliable way to make WKWebView transparent
                            let key: *mut Object = msg_send![class!(NSString), stringWithUTF8String: b"drawsBackground\0".as_ptr()];
                            let no_val: *mut Object = msg_send![class!(NSNumber), numberWithBool: NO];
                            let _: () = msg_send![wk_webview, setValue: no_val forKey: key];

                            eprintln!("[vibrancy] Set WKWebView drawsBackground = false");
                        }
                    });
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            check_cli_available,
            set_vibrancy,
            open_devtools,
            copy_image_to_clipboard,
            pick_directory,
            restart_app,
            plugin_commands::list_plugins,
            plugin_commands::list_global_skills,
            plugin_commands::list_global_rules,
            plugin_commands::install_plugin,
            plugin_commands::uninstall_plugin,
            plugin_commands::refresh_plugin,
            plugin_commands::set_plugin_auto_update,
            plugin_commands::set_agent_plugin_enabled,
            plugin_commands::set_agent_skill_subset,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
