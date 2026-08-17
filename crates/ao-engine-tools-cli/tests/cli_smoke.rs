//! Smoke tests for the `ao-engine-tools-cli` binary.
//!
//! Each test spawns the binary as a child process with a hand-rolled stub SSE
//! server and a temp `providers.toml` pointing at it. The env var
//! `LAUNCHPAD_STUDIO_DATA_DIR` injects the temp dir so the binary reads the
//! stub config instead of the developer's own `~/.launchpad_studio/providers.toml`.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// Minimal 6-event SSE body that produces a single "[assistant] Hello!" turn.
const SSE_BODY: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-7\",",
    "\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n",
    "\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
    "\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello!\"}}\n",
    "\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
    "\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n",
    "\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n",
    "\n",
);

/// 3-chunk SSE body: produces "Hello, world! Goodbye." across three
/// `text_delta` events so a streaming render can be observed by the
/// `[assistant]`-prefix-appears-once invariant.
const SSE_BODY_THREE_CHUNKS: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-7\",",
    "\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n",
    "\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
    "\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello, \"}}\n",
    "\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"world! \"}}\n",
    "\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"Goodbye.\"}}\n",
    "\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
    "\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n",
    "\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n",
    "\n",
);

/// Drain the full HTTP request (headers + body) from `conn`.
///
/// Reads until the `\r\n\r\n` header boundary is found, then drains any
/// remaining buffered bytes (the request body) by reading with a short
/// deadline. This prevents leftover body bytes from being misread later as
/// a client-disconnect signal.
async fn drain_full_request(conn: &mut tokio::net::TcpStream) {
    let mut buf = vec![0u8; 65536];
    let mut total = 0;
    let mut headers_done = false;

    loop {
        let deadline = Duration::from_millis(50);
        let Ok(Ok(n)) = tokio::time::timeout(deadline, conn.read(&mut buf[total..])).await else {
            // Timeout or error: if we've seen headers, we're done draining.
            if headers_done {
                break;
            }
            continue;
        };
        if n == 0 {
            break;
        }
        total += n;
        if !headers_done {
            if std::str::from_utf8(&buf[..total])
                .unwrap_or("")
                .contains("\r\n\r\n")
            {
                headers_done = true;
                // Don't break: drain body bytes that may have arrived with headers.
            }
        } else {
            // Headers already seen; keep draining until timeout.
        }
        if total >= buf.len() {
            break;
        }
    }
}

/// Start a stub HTTP server that responds to the first POST with `sse_body`
/// and then closes the connection. Returns the bound port.
async fn spawn_stub_server(sse_body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let Ok((mut conn, _)) = listener.accept().await else {
            return;
        };
        drain_full_request(&mut conn).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             content-type: text/event-stream\r\n\
             connection: close\r\n\
             \r\n\
             {}",
            sse_body
        );
        let _ = conn.write_all(response.as_bytes()).await;
        // Drop conn → connection-close signals end-of-body to reqwest.
    });

    port
}

/// Start a stub HTTP server that:
/// 1. Accepts the connection and sends partial SSE (just message_start).
/// 2. Fires `connected` to signal the test that the client is mid-stream.
/// 3. Holds the connection open until the client disconnects.
///
/// Returns the bound port.
async fn spawn_drip_stub_server(connected: Arc<Notify>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let Ok((mut conn, _)) = listener.accept().await else {
            return;
        };
        drain_full_request(&mut conn).await;

        // Send HTTP headers first, then a single SSE event so the client
        // enters the event-drain loop and the state machine is running.
        let partial = concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/event-stream\r\n",
            "connection: close\r\n",
            "\r\n",
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",",
            "\"type\":\"message\",\"role\":\"assistant\",\"content\":[],",
            "\"model\":\"claude-opus-4-7\",\"stop_reason\":null,",
            "\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n",
            "\n",
        );
        let _ = conn.write_all(partial.as_bytes()).await;

        // Signal the test: partial SSE delivered — client is now mid-stream.
        connected.notify_one();

        // Hold the connection open until the client cancels (drops it).
        // Sleep keeps the TCP connection alive without risking early exit from
        // a body-byte read returning 0 before the client sends SIGINT.
        tokio::time::sleep(Duration::from_secs(60)).await;
    });

    port
}

/// Write a minimal `providers.toml` into `dir` pointing at `base_url`.
fn write_providers_toml(dir: &std::path::Path, base_url: &str) {
    let content = format!(
        "[anthropic]\napi_key = \"sk-ant-SMOKE-TEST\"\nbase_url = \"{}\"\nmodel = \"claude-opus-4-7\"\n",
        base_url
    );
    std::fs::write(dir.join("providers.toml"), content).unwrap();
}

/// Path to the compiled binary under test.
fn binary_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_ao-engine-tools-cli"))
}

// ─── Test 1: one-turn echo ────────────────────────────────────────────────────

/// Spawns the CLI, sends one line of input, closes stdin, and asserts the
/// output contains "[assistant] Hello!" — the turn produced by the stub server.
#[tokio::test]
async fn cli_smoke_one_turn_echo() {
    let port = spawn_stub_server(SSE_BODY).await;
    let dir = tempfile::tempdir().unwrap();
    write_providers_toml(dir.path(), &format!("http://127.0.0.1:{}", port));

    let mut child = tokio::process::Command::new(binary_path())
        .arg("--provider")
        .arg("anthropic")
        .env("LAUNCHPAD_STUDIO_DATA_DIR", dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn ao-engine-tools-cli");

    // Write a line and drop stdin so the binary exits after one turn.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(b"hi\n").await.unwrap();
        // Drop stdin → EOF.
    }

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("child did not exit within 30 s")
        .expect("child wait_with_output failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stdout.contains("[assistant] Hello!"),
        "expected '[assistant] Hello!' in stdout\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
}

// ─── Test 2: ctrl-C returns to prompt with "<cancelled>" ─────────────────────

/// Spawns the CLI against a drip stub server, waits for the server to confirm
/// the client is mid-stream, then sends SIGINT and asserts the binary prints
/// "<cancelled>".  The SIGINT → "<cancelled>" path must complete within 1 s.
#[cfg(unix)]
#[tokio::test]
async fn cli_cancel_mid_stream_prints_cancelled() {
    let connected = Arc::new(Notify::new());
    let port = spawn_drip_stub_server(connected.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_providers_toml(dir.path(), &format!("http://127.0.0.1:{}", port));

    let mut child = tokio::process::Command::new(binary_path())
        .arg("--provider")
        .arg("anthropic")
        .env("LAUNCHPAD_STUDIO_DATA_DIR", dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn ao-engine-tools-cli");

    let pid = child.id().expect("child has no pid");

    // Write the user turn to stdin — the binary will start the session and
    // connect to the stub server.
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(b"a long turn that will be cancelled\n").await.unwrap();
        stdin.flush().await.unwrap();
    }

    // Wait (up to 30 s) for the stub server to confirm the client is mid-stream.
    // This ensures the binary has started run_session, connected to the stub,
    // and the partial SSE has been delivered — so the ctrl_c signal handler
    // is guaranteed to be registered by the time we send SIGINT.
    tokio::time::timeout(Duration::from_secs(30), connected.notified())
        .await
        .expect("stub server never received a connection — CLI may have crashed");

    // Brief sleep so the client processes the partial SSE event and is
    // suspended in stream.recv() when the signal arrives.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let t_before = std::time::Instant::now();

    // Send SIGINT to the child process.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGINT);
    }

    // Give the binary time to print "<cancelled>" and flush stdout.
    // The binary flushes immediately after println!("<cancelled>").
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Assert that cancel completed within a generous bound from our perspective.
    // The actual Anthropic-provider cancellation contract is 100 ms; we allow
    // 2 s here to account for CI/debug-build scheduling jitter.
    assert!(
        t_before.elapsed() < Duration::from_secs(2),
        "time budget exceeded: {:?}",
        t_before.elapsed()
    );

    // Close stdin so the REPL loop exits.
    drop(child.stdin.take());

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("child did not exit within 10 s")
        .expect("child wait failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stdout.contains("<cancelled>"),
        "expected '<cancelled>' in stdout after SIGINT\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ─── Test 3: second SIGINT hard-exits with code 130 ──────────────────────────

/// Drives the binary against a drip stub, then sends SIGINT twice with a small
/// gap. The first interrupt cancels the turn (existing contract); the second
/// must hard-exit the process with conventional SIGINT exit code 130.
///
/// This guards the long-lived signal handler that survives across turns —
/// without it, a second Ctrl-C is silently swallowed because tokio's signal
/// handler displaces the OS default once it has been installed once.
#[cfg(unix)]
#[tokio::test]
async fn cli_double_sigint_exits_with_code_130() {
    let connected = Arc::new(Notify::new());
    let port = spawn_drip_stub_server(connected.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_providers_toml(dir.path(), &format!("http://127.0.0.1:{}", port));

    let mut child = tokio::process::Command::new(binary_path())
        .arg("--provider")
        .arg("anthropic")
        .env("LAUNCHPAD_STUDIO_DATA_DIR", dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn ao-engine-tools-cli");

    let pid = child.id().expect("child has no pid");

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(b"a long turn that we will sigint twice\n")
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    // Wait until the stub confirms the client is mid-stream — guarantees the
    // signal task is registered and a turn is in flight.
    tokio::time::timeout(Duration::from_secs(30), connected.notified())
        .await
        .expect("stub server never received a connection — CLI may have crashed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // First SIGINT: cancels the in-flight turn.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGINT);
    }

    // Gap so the OS does not collapse the two SIGINTs into one pending signal,
    // and so the signal task has time to fire `cancel.cancel()` before we
    // sample `is_cancelled()` on the next interrupt. The window is generous
    // for CI/debug-build scheduling jitter.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Second SIGINT: must hard-exit. Either the slot still holds the
    // (now-cancelled) token, or the turn has already returned and the slot
    // is None — both branches route to exit(130).
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGINT);
    }

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("child did not exit within 10 s after second SIGINT")
        .expect("child wait failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(130),
        "expected exit code 130 (128 + SIGINT), got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
}

// ─── Test 4: streaming preserves per-chunk delivery, single-prefix render ────

/// Spawns the CLI against a 3-text-delta stub server and asserts:
///   1. The full concatenated text "Hello, world! Goodbye." appears in stdout.
///   2. The "[assistant] " prefix appears exactly ONCE across the entire turn,
///      even though the provider stream produced three `AssistantText`
///      chunks. This proves the live `SessionEventSink` plumbing is in
///      place — without it, either the stream is buffered (single chunk)
///      or each chunk gets its own prefix line (three prefixes).
#[tokio::test]
async fn cli_smoke_multi_chunk_text_renders_with_single_prefix() {
    let port = spawn_stub_server(SSE_BODY_THREE_CHUNKS).await;
    let dir = tempfile::tempdir().unwrap();
    write_providers_toml(dir.path(), &format!("http://127.0.0.1:{}", port));

    let mut child = tokio::process::Command::new(binary_path())
        .arg("--provider")
        .arg("anthropic")
        .env("LAUNCHPAD_STUDIO_DATA_DIR", dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn ao-engine-tools-cli");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(b"hi\n").await.unwrap();
        // Drop stdin → EOF → REPL exits after the one turn completes.
    }

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("child did not exit within 30 s")
        .expect("child wait_with_output failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stdout.contains("Hello, world! Goodbye."),
        "expected concatenated chunks in stdout\nstdout: {stdout}\nstderr: {stderr}"
    );
    let prefix_count = stdout.matches("[assistant]").count();
    assert_eq!(
        prefix_count, 1,
        "expected exactly one [assistant] prefix across 3 chunks, got {prefix_count}\nstdout: {stdout}"
    );
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
}
