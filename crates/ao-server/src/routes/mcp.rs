use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use ao_engine::agent_runner::{compute_tool_admission, is_channel_bridge_thread, CHANNEL_BLOCKED_TOOLS};
use ao_engine::event_bus::EventBusAgentSink;
use ao_engine::{AppState, LiveFormBridge};
use ao_engine_tools_core::{EventSink, FormBridge, WorkflowRunnerHandle};
use ao_mcp_bridge::{handle_request, JsonRpcRequest, JsonRpcResponse};

/// Keepalive comment interval for SSE-mode tools/call responses.
///
/// While a long-running tool is executing, the server emits a `: keepalive`
/// SSE comment every this-many seconds to prevent the TCP connection from
/// being silently dropped by NATs or load-balancers and to demonstrate that
/// the server is still alive to the MCP client's transport layer.
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Return `true` when the `Accept` header on a request includes
/// `text/event-stream`, indicating the caller can consume a streaming
/// response rather than a single buffered JSON body.
fn accepts_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false)
}

/// Resurrection metadata persisted by the spawn path alongside the per-spawn
/// MCP config file (see `prepare_mcp_session_with_chains`). Optional fields
/// keep older sidecars readable if the shape grows.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMetaSidecar {
    cwd: Option<String>,
    #[serde(default)]
    delegate_chain: Vec<String>,
    #[serde(default)]
    spawn_chain: Vec<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    floor_ts: Option<chrono::DateTime<chrono::Utc>>,
}

/// Attempt to rebuild a session entry lost to a server restart.
///
/// Preconditions, in order:
/// 1. `session_id` must parse as a UUID — it is interpolated into a file
///    path below, so this doubles as a path-traversal guard.
/// 2. The per-spawn config file `{agents_dir}/{agent_id}/mcp-{session_id}.json`
///    must still exist. The spawn guard deletes it the moment the subprocess
///    exits, so its presence means the spawn that minted this session id is
///    still running and legitimately calling back.
///
/// Session state (cwd, project scoping, delegation chains, transcript window
/// floor) is restored from the `mcp-{session_id}.meta.json` sidecar when
/// present. Spawns predating the sidecar fall back to the profile working
/// dir with empty chains — degraded (a project-scoped session loses its
/// project event routing) but functional.
async fn resurrect_session(
    state: &Arc<AppState>,
    profile: &ao_protocol::agent::AgentProfile,
    agent_id: &str,
    session_id: &str,
) -> Option<Arc<ao_engine::mcp_session::McpAgentSession>> {
    if Uuid::parse_str(session_id).is_err() {
        return None;
    }
    let agent_dir = state.persistence.data_root.agents_dir().join(agent_id);
    let config_path = agent_dir.join(format!("mcp-{session_id}.json"));
    if !tokio::fs::try_exists(&config_path).await.unwrap_or(false) {
        return None;
    }

    let meta_path = agent_dir.join(format!("mcp-{session_id}.meta.json"));
    let meta: Option<SessionMetaSidecar> = match tokio::fs::read(&meta_path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).ok(),
        Err(_) => None,
    };
    if meta.is_none() {
        tracing::warn!(
            agent_id = %agent_id,
            session_id = %session_id,
            "resurrecting MCP session without metadata sidecar; \
             cwd/project scoping restored from profile defaults"
        );
    }
    let meta = meta.unwrap_or(SessionMetaSidecar {
        cwd: None,
        delegate_chain: vec![],
        spawn_chain: vec![],
        project_id: None,
        thread_id: None,
        floor_ts: None,
    });
    let cwd = meta
        .cwd
        .map(std::path::PathBuf::from)
        .or_else(|| profile.working_dir.clone().map(std::path::PathBuf::from))
        .unwrap_or_else(|| state.persistence.data_root.root().join("tasks"));

    let session = match state.mcp_sessions.register_session_with_chains(
        session_id.to_string(),
        agent_id.to_string(),
        cwd,
        None,
        meta.delegate_chain,
        meta.spawn_chain,
        meta.project_id,
        meta.thread_id,
    ) {
        Ok(s) => s,
        // Duplicate-id race: a concurrent request resurrected it first.
        Err(()) => state.mcp_sessions.get_by_session_id(session_id)?,
    };
    if let Some(ts) = meta.floor_ts {
        state.mcp_sessions.update_floor(session_id, ts).await;
    }
    tracing::warn!(
        agent_id = %agent_id,
        session_id = %session_id,
        project_id = ?session.project_id,
        "resurrected MCP session after registry loss (server restart)"
    );
    Some(session)
}

/// POST /mcp/:agent_id/:session_id — MCP JSON-RPC endpoint for CLI-spawned agents.
pub async fn handle_mcp_request(
    State(state): State<Arc<AppState>>,
    Path((agent_id, session_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Look up agent profile — 404 if not found.
    let profile = match state.persistence.agents.get(&agent_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            let body = axum::Json(serde_json::json!({ "error": format!("agent not found: {agent_id}") }));
            return (StatusCode::NOT_FOUND, body).into_response();
        }
        Err(e) => {
            let body = axum::Json(serde_json::json!({ "error": e.to_string() }));
            return (StatusCode::INTERNAL_SERVER_ERROR, body).into_response();
        }
    };

    // Look up session by session_id. On a miss, attempt resurrection: a
    // server restart wipes the in-memory session store, but CLI subprocesses
    // spawned before the restart survive and keep calling back with their
    // original session ids. The per-spawn config file (deleted by the spawn
    // guard the moment the subprocess exits) proves such a spawn is still
    // alive, and its metadata sidecar carries the state needed to rebuild
    // the entry. Anything else is a true 404 — never register arbitrary ids.
    let session = match state.mcp_sessions.get_by_session_id(&session_id) {
        Some(s) => s,
        None => match resurrect_session(&state, &profile, &agent_id, &session_id).await {
            Some(s) => s,
            None => {
                let body = axum::Json(serde_json::json!({
                    "error": format!("session not found: {session_id}")
                }));
                return (StatusCode::NOT_FOUND, body).into_response();
            }
        },
    };

    // Validate agent_id matches the session's recorded agent — 400 on mismatch.
    if session.agent_id != agent_id {
        let body = axum::Json(serde_json::json!({
            "error": format!(
                "agent_id mismatch: URL has '{}' but session belongs to '{}'",
                agent_id, session.agent_id
            )
        }));
        return (StatusCode::BAD_REQUEST, body).into_response();
    }

    // Deserialize request body as JSON-RPC — 400 on parse error.
    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(
                agent_id = %agent_id,
                session_id = %session_id,
                error = %e,
                "MCP request body is not valid JSON-RPC"
            );
            let body = axum::Json(serde_json::json!({ "error": format!("invalid JSON-RPC request: {e}") }));
            return (StatusCode::BAD_REQUEST, body).into_response();
        }
    };

    let tool_name = (req.method == "tools/call")
        .then(|| req.params.as_ref())
        .flatten()
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());
    tracing::info!(
        agent_id = %agent_id,
        session_id = %session_id,
        method = %req.method,
        tool_name = tool_name,
        "MCP request received"
    );

    // Seed value for the constructor only; the real cwd is bound below as a
    // shared Arc via `with_cwd_arc` so a `Bash` `cd` (or worktree switch) in one
    // JSON-RPC call persists to the session entry and is observed by the next
    // call. Reading the current value here keeps the seed sensible even before
    // the Arc rebind takes effect.
    let cwd = session.cwd.read().unwrap().clone();
    let window_floor_ts = *session.window_floor_ts.read().await;

    // Build a per-request RunnerContext using a fresh UUID distinct from session_id.
    let run_ctx_id = Uuid::new_v4().to_string();
    // Events from project-scoped sessions route to the project channel
    // (`project:{id}`), matching where the CLI runner already publishes the
    // run-level events for project runs. The frontend subscribes to that
    // channel via `GET /projects/{id}/stream`, so tool-emitted events (form
    // requests, briefs, tool progress) surface in the project chat instead of
    // leaking into the per-agent chat. Payloads that need the real agent id
    // (e.g. `FormRequest.agent_id`, used to POST the answer back to
    // `/agents/{id}/form-answer`) carry it inside the payload, so channel
    // routing and answer delivery stay independent.
    let event_channel = match session.project_id {
        Some(ref pid) => format!("project:{pid}"),
        None => agent_id.clone(),
    };
    let event_sink = Arc::new(EventBusAgentSink {
        bus: Arc::clone(&state.event_bus),
        agent_id: event_channel,
        thread_id: session.thread_id.clone(),
    });

    // Resolve this session's thread once, up front: both the tool-admission
    // gate built below and the form-bridge interactivity choice right after
    // it need to know whether this turn originated from a channel-bridge
    // thread (Telegram, Discord, email, Slack, ...), and `offers_rename_tool`
    // further down reuses this same fetch instead of hitting the thread
    // store a second time.
    let session_thread = match session.thread_id {
        Some(ref tid) => state.persistence.threads.get(tid).await.ok().flatten(),
        None => None,
    };
    let on_channel_bridge = is_channel_bridge_thread(
        &profile,
        session.thread_id.as_deref(),
        session_thread.as_ref().and_then(|t| t.channel_origin.as_ref()),
    );

    // Wire a live form bridge so AskUserQuestionWithForm can suspend and render
    // in the app for CLI-spawned agents. The in-app native runner wires an
    // equivalent bridge per run; the MCP path needs its own, scoped to this
    // request. The bridge emits `FormRequest` through the same event sink the
    // frontend already subscribes to, and is keyed by `agent_id` in the shared
    // registry so `POST /agents/{id}/form-answer` can deliver the operator's
    // answer back to the suspended tool. Removed after dispatch returns.
    //
    // A channel-bridge turn has no UI to render a form on, so the bridge is
    // built non-interactive: `ask_form` fails fast with `NoOperator` instead
    // of suspending on an answer nothing can ever deliver. This is a
    // defense-in-depth backstop — the tool-admission gate below already
    // keeps `AskUserQuestionWithForm` out of such a turn's tool list
    // entirely, so this path only matters if that gate is ever bypassed.
    let form_bridge = Arc::new(if on_channel_bridge {
        LiveFormBridge::new_non_interactive(Arc::clone(&event_sink) as Arc<dyn EventSink + Send + Sync>)
    } else {
        // Wire the session-scoped suspension counter (shared across every
        // per-request bridge this session mints, and with the CLI
        // continuation loop's `SpawnInput.form_suspended` for the currently
        // running subprocess step — see `McpAgentSession::form_suspended`)
        // so a synchronous form suspends the overall wall-clock deadline
        // instead of silently burning it toward a SIGKILL.
        LiveFormBridge::new(Arc::clone(&event_sink) as Arc<dyn EventSink + Send + Sync>)
            .with_suspension_counter(Arc::clone(&session.form_suspended))
    });
    state
        .form_bridge_registry
        .register(&agent_id, Arc::clone(&form_bridge));

    // Backstop reaper: deregister this request's bridge from the shared
    // registry when the owning session ends, even if this request's own
    // future never reaches its normal tail cleanup below (e.g. the CLI
    // subprocess dies mid-suspension and the connection carrying this
    // request is torn down before `handle_request` returns). Detached via
    // `tokio::spawn` so it runs independently of this request's own
    // lifecycle — the whole point is to cover the case where nothing else
    // is left polling that lifecycle forward. Scoped to
    // `AskUserQuestionWithForm` calls (the only tool that suspends on this
    // bridge) so a session with heavy ordinary tool traffic doesn't
    // accumulate one watcher per call. `cancel_pending`/`deregister` are
    // both idempotent, so this is a harmless no-op on the (overwhelmingly
    // common) path where the request's own cleanup already ran first.
    if tool_name == Some("AskUserQuestionWithForm") {
        let reap_cancel = session.cancel.clone();
        let reap_state = Arc::clone(&state);
        let reap_agent_id = agent_id.clone();
        let reap_bridge = Arc::clone(&form_bridge);
        tokio::spawn(async move {
            reap_cancel.cancelled().await;
            reap_bridge.cancel_pending();
            reap_state
                .form_bridge_registry
                .deregister(&reap_agent_id, &reap_bridge);
        });
    }

    let wf_handle: Arc<dyn WorkflowRunnerHandle + Send + Sync> =
        Arc::clone(&state.workflow_runner) as Arc<dyn WorkflowRunnerHandle + Send + Sync>;

    let transcript_store = Arc::new(ao_persistence::transcript::TranscriptStore::new(
        state.persistence.data_root.clone(),
    ));

    // Load the skill registry from the same pools (user + enabled plugins) the
    // freshly-fetched profile advertises, overlaying any MCP server
    // prompt-sourced skills. The native in-app runner does this per run; the
    // MCP HTTP path must do it per request or `RunSkill`/`SkillRegister` would
    // resolve against an empty registry and 404 every skill — including ones
    // already enabled on the profile. Each request rebuilds the context, so the
    // load is fresh every time; a skill registered earlier in the session is
    // persisted to the profile on disk and picked up on the next request's load.
    // Shared with the native runner via build_skill_registry so neither path can
    // silently drift into the empty-registry bug again.
    let skill_registry = ao_engine::agent_context::build_skill_registry(
        state.persistence.data_root.root(),
        &profile,
        Some(&state.mcp_manager),
    );

    // Conditionally extend the shared static tool registry with per-request,
    // thread-eligibility-gated tools. `state.tools_registry` is one process-
    // wide `Arc` mutated in place for dynamic MCP-server tools, so it can't
    // be used directly to vary membership per session; cloning it here is
    // cheap (Arc-shared tool instances) and mirrors the same conditional
    // extension the native runner does in `agent_runner::native`. Every
    // JSON-RPC request already rebuilds its context from scratch (see the
    // comment on `skill_registry` above), so recomputing this per request
    // costs one extra in-memory thread-store lookup, not a new pattern.
    //
    // - `RenameThread`: only when the acting thread is eligible (personal,
    //   non-default, not yet named — see `Thread::offers_rename_tool`).
    // - `ListThreads`/`SummarizeThread`: only when this agent has more than
    //   one thread at all — mirrors the native runner's gate so CLI-mode
    //   agents get the same tool surface as API-mode agents instead of
    //   silently missing cross-thread tools.
    let offers_rename_tool = session_thread
        .as_ref()
        .map(|t| t.offers_rename_tool())
        .unwrap_or(false);
    let offers_cross_thread_tools = state
        .persistence
        .threads
        .list_for_agent(&agent_id)
        .await
        .unwrap_or_default()
        .len()
        > 1;
    let effective_tools_registry: Arc<ao_engine_tools_core::Registry> =
        if offers_rename_tool || offers_cross_thread_tools {
            let mut extended = (*state.tools_registry).clone();
            if offers_rename_tool {
                ao_engine_tools_engine::rename_thread::register(&mut extended);
            }
            if offers_cross_thread_tools {
                ao_engine_tools_engine::list_threads::register(&mut extended);
                ao_engine_tools_engine::summarize_thread::register(&mut extended);
            }
            Arc::new(extended)
        } else {
            Arc::clone(&state.tools_registry)
        };

    // Agent-level admission gate from the profile's `ToolsConfig`, with the
    // channel-blocked tools (e.g. `AskUserQuestionWithForm`) folded in on a
    // channel-bridge turn. Mirrors the native runner's identical gate in
    // `agent_runner::native` — this is the primary enforcement point for the
    // CLI/MCP path: `build_tool_specs` drops a denied tool before it can ever
    // be emitted to the model, deferred or not.
    let extra_deny: &[&str] = if on_channel_bridge { CHANNEL_BLOCKED_TOOLS } else { &[] };
    let tool_admission =
        compute_tool_admission(profile.tools.as_ref(), &effective_tools_registry, extra_deny);

    let mut ctx = ao_engine_tools_core::context::RunnerContext::new_with_cwd(
        run_ctx_id,
        agent_id.clone(),
        cwd,
    )
    .with_depth(session.delegation_depth as usize)
    .with_delegate_chain(session.delegate_chain.clone())
    .with_spawn_chain(session.spawn_chain.clone())
    // Bind this per-request context's cancellation token to the session's own
    // — without this, `RunnerContext::new`'s default (a private token nobody
    // else holds) leaves the cancel arm of every `tokio::select!` racing
    // `ctx.cancel.cancelled()` permanently dead for CLI-spawned agents (e.g.
    // `AskUserQuestionWithForm`'s sync-form wait). `McpSessionStore::remove`
    // cancels `session.cancel` as part of tearing the session down —
    // subprocess exit, the `/sessions` DELETE route, and the TTL sweep all
    // funnel through it — so a tool call suspended on this session resolves
    // as cancelled instead of only ever unblocking via its own deadline.
    .with_cancel(session.cancel.clone())
    // This context lives for exactly one tool call and is dropped on return, so
    // nothing here ever drains `pending_user_messages`. An inline skill must
    // therefore deliver its body through the tool result — the only channel the
    // externally-driven CLI agent observes — rather than enqueuing it.
    .with_inline_skill_via_tool_result()
    // Bind this per-request context to the session-scoped read snapshots so a
    // `Read` from an earlier JSON-RPC call in this session is visible to an
    // `Edit`/`Write` here. Without this, every request mints a fresh empty
    // `ReadFileState` and the read-before-write guard rejects the edit with
    // "File has not been read yet". The native runner gets this for free by
    // keeping one long-lived context per run.
    .with_read_file_state_arc(Arc::clone(&session.read_file_state))
    // Bind this per-request context to the session-scoped cwd Arc so a `Bash`
    // `cd` or an `EnterWorktree`/`ExitWorktree` in one JSON-RPC call propagates
    // to the session entry and is visible to the next call. Without this every
    // request resets cwd to the registration-time value (the constructor seed),
    // so relative-path tools after a `cd` would resolve against the wrong
    // directory. The native runner gets this for free by keeping one long-lived
    // context per run; the same Arc-share trick as `read_file_state` above.
    .with_cwd_arc(Arc::clone(&session.cwd))
    .with_registry(Arc::clone(&effective_tools_registry))
    .with_tool_admission(tool_admission)
    .with_thread_store(Arc::clone(&state.persistence.threads))
    .with_skill_registry(skill_registry)
    .with_event_sink(event_sink)
    .with_workflow_runner(wf_handle)
    .with_transcript_store(transcript_store)
    // Mirrors the native runner's wiring (agent_runner::native) — without this
    // the MCP/CLI request path leaves `ctx.assignment_store` at its `None`
    // default and every Assignment* tool call fails with "Assignment store
    // not available in this context", even for the top-level agent.
    .with_assignment_store(Arc::clone(&state.persistence.assignments))
    .with_assignment_fire(Arc::clone(&state.assignment_fire))
    // Needed so a posted async form can record a `pending_forms` entry on the
    // agent snapshot — the gate the composer uses to swap the text input for the form.
    .with_snapshot_store(Arc::clone(&state.persistence.snapshots))
    .with_memory_store(Arc::clone(&state.persistence.memory))
    // Mirrors the native runner's wiring — without this the MCP/CLI request
    // path leaves `ctx.reflection_staging` at its `None` default, so every
    // project/global MemoryWrite silently no-ops on persistence while still
    // reporting `staged: true`, leaving the review queue permanently empty.
    .with_reflection_staging(Arc::clone(&state.persistence.reflection_staging))
    .with_artifact_store(Arc::clone(&state.persistence.artifacts))
    .with_tasklist_service(
        Arc::clone(&state.tasklist_service)
            as Arc<dyn ao_engine_tools_core::TasklistServiceHandle + Send + Sync>,
    )
    // Plumb the classifier through so Todo* tool calls (TodoCreate /
    // TodoUpdate) can fire background classifications immediately at
    // tasklist-mutation time. Without this, agent-owned tasks created via
    // the MCP path land with `assignment: None` and wait for the next
    // reconciler tick before getting routed.
    .with_classifier(Arc::clone(&state.task_classifier_handle))
    // Share the same in-flight dedup set the reconciler uses so an MCP
    // spawn can't collide with a reconciler tick on the same task.
    .with_classifier_in_flight(Arc::clone(&state.classifier_in_flight))
    // Lets Todo* tools resolve an `owner` value (agent_id or address-book
    // display name) to a canonical agent_id at task-creation/update time,
    // the same lookup `Delegate.target` performs.
    .with_agent_profile_store(Arc::new(ao_persistence::profiles::AgentProfileStore::new(
        state.persistence.data_root.clone(),
    )))
    .with_form_bridge(Arc::clone(&form_bridge) as Arc<dyn FormBridge + Send + Sync>)
    // Share the session-scoped background-agent registry so handles inserted
    // by Delegate mode=async in one JSON-RPC call are visible to DelegateOutput
    // in a subsequent call within the same session.
    .with_background_agents(Arc::clone(&session.background_agents))
    // Same reason, for background shell commands: without this bind, the id a
    // `Bash { run_in_background: true }` call returns dies with that request's
    // context, and the follow-up `BashStatus`/`BashKill` rejects it as unknown
    // while the subprocess keeps running unsupervised.
    .with_background_commands(Arc::clone(&session.background_commands));

    // Attach the delegate-completion sink so an async Delegate tool call from
    // this request notifies the parent agent via its durable queue when the
    // background delegate finishes.  Uses the same queue mechanism as tasklist
    // completion — the parent agent wakes up on its next queue-pump cycle
    // rather than having to poll via DelegateOutput.
    let mut delegate_sink =
        ao_engine::delegate_completion::QueueDelegateCompletionSink::new(
            Arc::clone(&state.queue_managers)
                as Arc<dyn ao_engine::queue_manager::NotificationDispatcher>,
            agent_id.clone(),
        )
        .with_event_bus(Arc::clone(&state.event_bus))
        .with_data_root(state.persistence.data_root.clone())
        .with_thread_store(Arc::clone(&state.persistence.threads));
    if let Some(ref pid) = session.project_id {
        delegate_sink = delegate_sink.with_project_id(pid.clone());
    }
    if let Some(ref tid) = session.thread_id {
        delegate_sink = delegate_sink.with_thread_id(tid.clone());
    }
    ctx = ctx.with_delegate_completion_sink(Arc::new(delegate_sink));

    if let Some(ts) = window_floor_ts {
        ctx = ctx.with_window_floor_ts(ts);
    }

    // Project wiring + verification-engine injection must run BEFORE the
    // `profile.workflows` binding below: that binding partially moves `profile`
    // (the `workflows` field), after which `&profile` can no longer be borrowed.
    // The verification builders need `&profile` to thread the coordinator's
    // provider/runner config into the engines, so this block is ordered first.
    if let Some(ref pid) = session.project_id {
        let project_store = Arc::new(ao_persistence::projects::ProjectStore::new(
            state.persistence.data_root.clone(),
        ));
        ctx = ctx.with_project(pid.clone()).with_project_store(project_store);
        // Inject the quick verification engine (mode='quick') for mid-flight
        // gap checks. Runs against the coordinator's own provider/runner config
        // (CLI binary or native API). Falls back gracefully when no provider is
        // configured.
        if let Some(engine) = ao_engine::build_quick_verification_engine(&profile) {
            ctx = ctx.with_verification_engine(engine);
        }
        // Inject the full inspection engine (mode='full') required by the
        // ProjectComplete gate. Uses the current session's tool registry so
        // the inspection child's filtered copy includes all registered tools.
        if let Some(engine) =
            ao_engine::build_full_verification_engine(&profile, ctx.registry.clone())
        {
            ctx = ctx.with_full_verification_engine(engine);
        }
    }

    // Only bother building the summarization engine when `SummarizeThread` was
    // actually registered above (`offers_cross_thread_tools`, computed with
    // `effective_tools_registry`) — otherwise it's unreachable this request and
    // building it would resolve a provider client for nothing. Mirrors the
    // native runner's identical gate in `agent_runner::native`.
    //
    // This was previously missing entirely on the CLI/MCP path: `ctx` never
    // got `with_thread_summarization_engine` set, so `SummarizeThread` always
    // hit its `ctx.thread_summarization_engine` `None` branch and reported a
    // spurious "no provider is configured" error — even though this same
    // `profile` is the one already driving the calling agent (and, for
    // `AgentRunnerMode::Cli`, needs no `providers.toml` entry at all, since
    // `build_thread_summarization_engine` shells out to the agent's own CLI
    // binary via `CliProviderClient`). Using `&profile` here — the same value
    // already threaded into `build_quick_verification_engine`/
    // `build_full_verification_engine` above — guarantees the summarizer runs
    // against the exact provider/model/runner-mode this agent is already
    // authenticated with, not a separate default.
    if offers_cross_thread_tools {
        if let Some(engine) = ao_engine::build_thread_summarization_engine(&profile) {
            ctx = ctx.with_thread_summarization_engine(engine);
        }
    }

    // Wire thread scope: TodoCreate/Delegate read ctx.thread_id to tag
    // completion events and persisted transcript markers with the thread that
    // was active when the tool call happened, instead of always falling back
    // to the agent's default-thread transcript.
    if let Some(ref tid) = session.thread_id {
        ctx = ctx.with_thread(tid.clone());
    }

    if let Some(binding) = profile.workflows {
        ctx = ctx.with_agent_workflows(binding);
    }

    // SSE response mode for tools/call: when the client advertises
    // text/event-stream in its Accept header, respond with headers immediately
    // (200 + Content-Type: text/event-stream) and stream the response body
    // lazily. This prevents the transport-level first-response-byte timeout
    // (typically ~60 s on HTTP clients) from killing long-running synchronous
    // tool calls before the result arrives.
    //
    // While the tool future runs, the server emits periodic keepalive comment
    // lines. When the future resolves, the complete JSON-RPC response (success
    // or error) is sent as a single SSE data event and the stream closes.
    // Non-tools/call methods and plain-JSON-only callers keep the existing
    // buffered path unchanged.
    if req.method == "tools/call" && accepts_event_stream(&headers) {
        // Extract a progress token from _meta if the caller provided one.
        // When present, each keepalive tick also emits a JSON-RPC
        // notifications/progress message so clients that track it can
        // show in-flight progress without polling.
        let progress_token = req
            .params
            .as_ref()
            .and_then(|p| p.get("_meta"))
            .and_then(|m| m.get("progressToken"))
            .cloned();

        // Channel that feeds the SSE response body. The spawned driver task
        // writes keepalives and the final result; the ReceiverStream is given
        // to axum as the SSE body stream.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);

        // Capture Arcs for cleanup inside the spawned task; these are cheap
        // pointer-width increments.
        let registry_arc = Arc::clone(&effective_tools_registry);
        let sse_state = Arc::clone(&state);
        let sse_session = Arc::clone(&session);
        let sse_agent_id = agent_id.clone();
        let sse_form_bridge = Arc::clone(&form_bridge);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(SSE_KEEPALIVE_INTERVAL);
            // Consume the immediate first tick so the first keepalive fires
            // after one full interval, not at t=0.
            interval.tick().await;
            let mut progress_count: u32 = 0;

            // Run the tool concurrently with the keepalive ticker.
            let tool_fut = handle_request(req, &*registry_arc, &ctx);
            tokio::pin!(tool_fut);

            let result = loop {
                tokio::select! {
                    resp = &mut tool_fut => break resp,
                    _ = interval.tick() => {
                        // SSE comment line: ignored by the JSON-RPC layer,
                        // just keeps the transport connection alive.
                        let _ = tx.send(Ok(Event::default().comment("keepalive"))).await;

                        // Optional: forward as a JSON-RPC progress notification
                        // when the request carried a progressToken.
                        if let Some(ref token) = progress_token {
                            progress_count += 1;
                            let notif = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": {
                                    "progressToken": token,
                                    "progress": progress_count,
                                }
                            });
                            let notif_str = serde_json::to_string(&notif)
                                .unwrap_or_default();
                            let _ = tx.send(Ok(Event::default().data(notif_str))).await;
                        }
                    }
                }
            };

            // Cleanup mirrors the plain-JSON path: cancel any suspended form
            // future, deregister the bridge so no stale entries leak, and
            // update the session's last-seen timestamp.
            sse_form_bridge.cancel_pending();
            sse_state
                .form_bridge_registry
                .deregister(&sse_agent_id, &sse_form_bridge);
            *sse_session.last_seen_at.write().await = Instant::now();

            // Send the complete JSON-RPC response (success or error) as the
            // final SSE data event, then let tx drop to signal end-of-stream.
            if let Some(r) = result {
                let data = serde_json::to_string(&r).unwrap_or_default();
                let _ = tx.send(Ok(Event::default().data(data))).await;
            }
            // tx drops here — ReceiverStream sees the channel close and axum
            // terminates the SSE response body.
        });

        return Sse::new(ReceiverStream::new(rx)).into_response();
    }

    // Plain-JSON path (all non-tools/call methods, or tools/call callers that
    // did not advertise text/event-stream). Behaviour is unchanged from the
    // original implementation.
    //
    // Captured before the move into `handle_request` below — needed to gate
    // the tools/list response log after `resp` comes back.
    let is_tools_list = req.method == "tools/list";

    // Dispatch to the MCP bridge handler. If this call was an
    // AskUserQuestionWithForm invocation, it suspends here until the operator
    // submits the form (delivered via the form_bridge_registry entry above) or
    // the session is cancelled.
    let resp: Option<JsonRpcResponse> =
        handle_request(req, &effective_tools_registry, &ctx).await;

    // Drop any pending senders and deregister the bridge for this request so a
    // dropped/cancelled form future can't leak a stale registry entry.
    form_bridge.cancel_pending();
    state
        .form_bridge_registry
        .deregister(&agent_id, &form_bridge);

    // Update last_seen_at after every successful dispatch.
    *session.last_seen_at.write().await = Instant::now();

    // Diagnostic for whether the tool surface a CLI-spawned agent discovers is
    // actually populated: logs the response side of tools/list separately from
    // the "MCP request received" log above (which only covers the request).
    // Not logged for other methods — a tools/call result can carry an
    // arbitrarily large payload we don't want in the log stream.
    if is_tools_list {
        if let Some(ref r) = resp {
            let tools = r.result.as_ref().and_then(|v| v.get("tools")).and_then(|t| t.as_array());
            let tool_count = tools.map(|arr| arr.len()).unwrap_or(0);
            let tool_names = tools
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            tracing::info!(
                agent_id = %agent_id,
                session_id = %session_id,
                tool_count,
                tool_names = %tool_names,
                "MCP tools/list response"
            );
        }
    }

    match resp {
        Some(r) => Json(r).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use std::collections::HashMap;

    fn make_agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: "Test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            max_turns: None,
        }
    }

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    /// Regression guard for the MCP/CLI request path's `RunnerContext` wiring:
    /// `handle_mcp_request` must plumb `reflection_staging` into the per-request
    /// context, mirroring the native runner. Without that wiring a project- or
    /// global-scope `MemoryWrite` reports `staged: true` yet silently drops the
    /// candidate, so the human review queue stays permanently empty. This drives
    /// a real global-scope `MemoryWrite` through the route and asserts the
    /// candidate actually lands in the reflection-staging store — it FAILS if the
    /// `.with_reflection_staging(...)` builder line is removed (staging store
    /// stays `None`, nothing is persisted, `list_pending` returns empty).
    #[tokio::test]
    async fn mcp_route_memory_write_stages_candidate_for_review() {
        let (state, _tmp) = setup_state().await;
        let agent_id = "agent-mcp-1";
        let session_id = "session-mcp-1";
        let agent = make_agent(agent_id);
        state.persistence.agents.create(&agent).await.unwrap();

        // Register the MCP session the route resolves by session_id, so the
        // request reaches the ctx-builder path under test rather than 404-ing.
        state
            .mcp_sessions
            .register_session_with_chains(
                session_id.to_string(),
                agent_id.to_string(),
                _tmp.path().to_path_buf(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .expect("register mcp session");

        // Global scope is a `StageForReview` decision (self-authored, non-agent
        // scope) that needs no git/project resolution — the lightest write that
        // still exercises the staging wiring.
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "MemoryWrite",
                "arguments": {
                    "scope": "global",
                    "content": "regression guard: mcp route must stage for review"
                }
            }
        });
        let body_bytes = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());

        let resp = handle_mcp_request(
            State(Arc::clone(&state)),
            Path((agent_id.to_string(), session_id.to_string())),
            HeaderMap::new(),
            body_bytes,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "tools/call dispatch should return 200"
        );

        let pending = state
            .persistence
            .reflection_staging
            .list_pending(agent_id)
            .await
            .expect("list_pending");
        assert_eq!(
            pending.len(),
            1,
            "a global MemoryWrite via the MCP route must stage exactly one review candidate"
        );
        assert_eq!(pending[0].agent_id, agent_id);
    }

    /// Fetch the `tools/list` result for a session and return the set of
    /// admitted tool names — the shared assertion helper for the two
    /// channel-bridge-gating regression tests below.
    async fn list_tool_names(
        state: &Arc<AppState>,
        agent_id: &str,
        session_id: &str,
    ) -> std::collections::HashSet<String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
        });
        let body_bytes = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());
        let resp = handle_mcp_request(
            State(Arc::clone(state)),
            Path((agent_id.to_string(), session_id.to_string())),
            HeaderMap::new(),
            body_bytes,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "tools/list dispatch should return 200");
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let json: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("tools/list response is valid JSON");
        json["result"]["tools"]
            .as_array()
            .expect("tools array present")
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// Part 1 regression guard: a channel-bridge session on the CLI/MCP path
    /// must never be served `AskUserQuestionWithForm` — it renders a UI form
    /// with no channel-side surface to draw on, and the backend would
    /// otherwise suspend forever waiting for an answer that can never arrive.
    /// A normal (non-bridge) session for the same agent must still see it.
    #[tokio::test]
    async fn mcp_route_denies_ask_form_tool_on_channel_bridge_session_but_not_elsewhere() {
        let (state, tmp) = setup_state().await;
        let agent_id = "agent-bridge-gate";
        let bridge_thread_id = "bridge-thread-1";

        let mut agent = make_agent(agent_id);
        agent.set_telegram_config(Some(ao_protocol::agent::TelegramConfig {
            enabled: true,
            bot_username: None,
            thread_mode: Default::default(),
            bridge_thread_id: Some(bridge_thread_id.to_string()),
            allowed_chat_ids: vec![],
            pending_pairing_code: None,
        }));
        state.persistence.agents.create(&agent).await.unwrap();

        // Session whose thread_id matches the enabled Telegram binding's
        // bridge_thread_id — this is the channel-bridge turn.
        let bridge_session_id = "session-bridge";
        state
            .mcp_sessions
            .register_session_with_chains(
                bridge_session_id.to_string(),
                agent_id.to_string(),
                tmp.path().to_path_buf(),
                None,
                vec![],
                vec![],
                None,
                Some(bridge_thread_id.to_string()),
            )
            .expect("register bridge mcp session");

        // Session with no thread_id at all — an ordinary desktop/API turn on
        // the same agent, which must be unaffected by the Telegram binding.
        let normal_session_id = "session-normal";
        state
            .mcp_sessions
            .register_session_with_chains(
                normal_session_id.to_string(),
                agent_id.to_string(),
                tmp.path().to_path_buf(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .expect("register normal mcp session");

        let bridge_tools = list_tool_names(&state, agent_id, bridge_session_id).await;
        assert!(
            !bridge_tools.contains("AskUserQuestionWithForm"),
            "channel-bridge session must not be served AskUserQuestionWithForm; got {bridge_tools:?}"
        );

        let normal_tools = list_tool_names(&state, agent_id, normal_session_id).await;
        assert!(
            normal_tools.contains("AskUserQuestionWithForm"),
            "non-bridge session for the same agent must still be served AskUserQuestionWithForm; got {normal_tools:?}"
        );
    }

    /// Wait (bounded) for a `FormRequest` event on the event bus — proof a
    /// sync form call has genuinely suspended, not merely started.
    /// `LiveFormBridge::ask_form` only emits this event after it has already
    /// registered the oneshot sender in its channel map, so observing it
    /// here guarantees the tool call under test is truly parked before the
    /// caller goes on to cancel or abandon it.
    async fn wait_for_form_request(
        events: &mut tokio::sync::broadcast::Receiver<ao_protocol::event::AgentEvent>,
    ) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Ok(evt)
                        if matches!(
                            evt.payload,
                            ao_protocol::event::AgentEventPayload::FormRequest { .. }
                        ) =>
                    {
                        return;
                    }
                    Ok(_) => continue,
                    Err(_) => panic!("event bus closed before FormRequest was observed"),
                }
            }
        })
        .await
        .expect("timed out waiting for FormRequest — tool call never suspended");
    }

    /// Shared test-only lock for the process-wide `AO_SYNC_FORM_TIMEOUT_SECS`
    /// env var (T2g). Unlike `LAUNCHPAD_STUDIO_DATA_DIR`
    /// (`crate::routes::env_lock::ENV_LOCK`), which only needs guarding at
    /// process/state *setup*, this var is re-read fresh on every single
    /// `AskUserQuestionWithForm` sync-mode dispatch via
    /// `ask_user_question_form::sync_form_timeout()` — so any test that
    /// overrides it must hold this lock for its whole dispatch-to-resolution
    /// window, and so must every *other* sync-form test in this module that
    /// never sets the var itself but still reads its (possibly-overridden)
    /// value through that same call, or `cargo test`'s default
    /// multi-threaded parallelism could let one test's override leak into
    /// another's dispatch.
    static SYNC_FORM_TIMEOUT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Reachability guard for the MCP route's cancellation wiring (T2c).
    ///
    /// Proves the LIVE route — not `resolve_sync_form` exercised directly —
    /// actually arrives at `ctx.cancel`'s select arm: cancels the session's
    /// own token exactly the way real session teardown does
    /// (`McpSessionStore::remove`, the single funnel every teardown path —
    /// subprocess exit, the `/sessions` DELETE route, the TTL sweep — goes
    /// through) and asserts a still-suspended sync form resolves as
    /// cancelled through the full HTTP dispatch. Before this wiring,
    /// `handle_mcp_request` never set `ctx.cancel`, so `RunnerContext`'s
    /// private default token left the cancel arm permanently dead for
    /// CLI-spawned agents; a regression back to that state makes this test
    /// hang until its own timeout rather than pass.
    #[tokio::test]
    async fn mcp_route_session_cancel_resolves_suspended_sync_form_as_cancelled() {
        let (state, tmp) = setup_state().await;
        // See `SYNC_FORM_TIMEOUT_ENV_LOCK`: this test dispatches a sync form,
        // which reads `AO_SYNC_FORM_TIMEOUT_SECS` on every call, so it must
        // hold the same lock the timeout-override test below uses to avoid
        // racing that test's mutation of this var.
        let _timeout_env_guard = SYNC_FORM_TIMEOUT_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let agent_id = "agent-cancel-reach";
        let session_id = "session-cancel-reach";
        let agent = make_agent(agent_id);
        state.persistence.agents.create(&agent).await.unwrap();
        state
            .mcp_sessions
            .register_session_with_chains(
                session_id.to_string(),
                agent_id.to_string(),
                tmp.path().to_path_buf(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .expect("register mcp session");

        let session = state.mcp_sessions.get_by_session_id(session_id).unwrap();
        assert!(
            !session.cancel.is_cancelled(),
            "a freshly registered session must start with a live, uncancelled token"
        );

        let mut events = state.event_bus.subscribe();

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "AskUserQuestionWithForm",
                "arguments": {
                    "title": "Reachability probe",
                    "mode": "sync",
                    "questions": [{"id": "x", "type": "text", "label": "L"}]
                }
            }
        });
        let body_bytes = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());

        let call_state = Arc::clone(&state);
        let call_agent_id = agent_id.to_string();
        let call_session_id = session_id.to_string();
        let call = tokio::spawn(async move {
            handle_mcp_request(
                State(call_state),
                Path((call_agent_id, call_session_id)),
                HeaderMap::new(),
                body_bytes,
            )
            .await
        });

        wait_for_form_request(&mut events).await;

        // The crux of the reachability assertion: cancel the session's own
        // token the way real teardown does — NOT `ctx.cancel` or the bridge
        // directly, and not some fresh, unrelated token.
        state.mcp_sessions.remove(session_id);

        let resp = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect(
                "handle_mcp_request did not return after the session's token \
                 was cancelled — ctx.cancel is not wired to the live session",
            )
            .expect("request task panicked");

        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("valid JSON");
        let result = &json["result"];
        assert_eq!(
            result["isError"], true,
            "a cancelled sync form must surface as isError:true, got {result:?}"
        );
        assert_eq!(result["content"][0]["text"], "cancelled");

        // The request's own tail cleanup ran (it was still live and being
        // polled, unlike the abandoned-future scenario the reap test below
        // covers) — confirm no bridge is left registered behind it.
        assert_eq!(
            state.form_bridge_registry.bridge_count(agent_id),
            0,
            "the cancelled request's own cleanup must deregister its bridge"
        );
    }

    /// Reachability guard for the sync-form TIMEOUT arm (T2g), symmetric with
    /// `mcp_route_session_cancel_resolves_suspended_sync_form_as_cancelled`
    /// above.
    ///
    /// That test proves `ctx.cancel` reaches the live route; this proves the
    /// deadline arm does too — that a sync form suspended through the real
    /// mounted route, left unanswered and uncancelled, actually resolves via
    /// `resolve_sync_form`'s own `tokio::time::sleep(timeout)` branch and
    /// `form_timed_out_output`, not merely via the unit tests that call
    /// `resolve_sync_form`/`invoke` directly
    /// (`ask_user_question_form::tests::invoke_sync_mode_surfaces_timeout_end_to_end`
    /// and its siblings). Overrides `AO_SYNC_FORM_TIMEOUT_SECS` — the same
    /// seam that crate-level test uses — down to 1s so this test runs in
    /// about a second instead of waiting out the real 1800s default; see
    /// `SYNC_FORM_TIMEOUT_ENV_LOCK` for why that override is guarded.
    ///
    /// Also proves the suspension counter (`McpAgentSession::form_suspended`,
    /// wired via `.with_suspension_counter` above) goes to 1 while the form
    /// is outstanding and back to 0 once it resolves via timeout —
    /// `FormSuspensionGuard` inside `LiveFormBridge::ask_form` clears from
    /// `Drop`, which fires whether that future completes normally or is
    /// dropped because the timeout branch won the race. A leaked nonzero
    /// counter would exclude all future wall-clock time on this session from
    /// the process supervisor's overall-timeout budget forever — the
    /// highest-consequence regression this test can catch.
    #[tokio::test]
    async fn mcp_route_sync_form_timeout_resolves_as_timed_out_and_clears_suspension_counter() {
        let (state, tmp) = setup_state().await;
        // See `SYNC_FORM_TIMEOUT_ENV_LOCK`. Held for the full dispatch, since
        // this test both mutates and reads `AO_SYNC_FORM_TIMEOUT_SECS`.
        let _timeout_env_guard = SYNC_FORM_TIMEOUT_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let agent_id = "agent-timeout-reach";
        let session_id = "session-timeout-reach";
        let agent = make_agent(agent_id);
        state.persistence.agents.create(&agent).await.unwrap();
        state
            .mcp_sessions
            .register_session_with_chains(
                session_id.to_string(),
                agent_id.to_string(),
                tmp.path().to_path_buf(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .expect("register mcp session");

        let session = state.mcp_sessions.get_by_session_id(session_id).unwrap();
        assert_eq!(
            session
                .form_suspended
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a freshly registered session must start with an idle suspension counter"
        );

        let mut events = state.event_bus.subscribe();

        // Short override so the deadline elapses in ~1s instead of the real
        // 1800s default — the same env-var seam
        // `ask_user_question_form::sync_form_timeout()` reads in production.
        std::env::set_var("AO_SYNC_FORM_TIMEOUT_SECS", "1");

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "AskUserQuestionWithForm",
                "arguments": {
                    "title": "Timeout probe",
                    "mode": "sync",
                    "questions": [{"id": "x", "type": "text", "label": "L"}]
                }
            }
        });
        let body_bytes = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());

        let call_state = Arc::clone(&state);
        let call_agent_id = agent_id.to_string();
        let call_session_id = session_id.to_string();
        let call = tokio::spawn(async move {
            handle_mcp_request(
                State(call_state),
                Path((call_agent_id, call_session_id)),
                HeaderMap::new(),
                body_bytes,
            )
            .await
        });

        wait_for_form_request(&mut events).await;

        // The form is genuinely suspended now. No operator answers it and no
        // one cancels the session, so the only way `resolve_sync_form`'s
        // `tokio::select!` can resolve from here is its own `timeout` branch.
        assert_eq!(
            session
                .form_suspended
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "suspension counter must go to 1 while the sync form is outstanding"
        );

        let resp = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect(
                "handle_mcp_request did not return after the overridden 1s \
                 sync-form deadline elapsed — the timeout arm is not reachable \
                 through the live route",
            )
            .expect("request task panicked");

        std::env::remove_var("AO_SYNC_FORM_TIMEOUT_SECS");

        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("valid JSON");
        let result = &json["result"];

        // Unlike the cancelled arm (`isError: true`, plain "cancelled" text),
        // a timed-out form is a structured, non-error outcome — see
        // `form_timed_out_output`'s doc comment for why: it must read as "the
        // deadline elapsed", not "the tool call itself failed, retry is
        // plausible".
        assert_eq!(
            result["isError"], false,
            "a timed-out sync form must not surface as isError:true, got {result:?}"
        );
        let text = result["content"][0]["text"]
            .as_str()
            .expect("structured timeout output must render as text content");
        let payload: serde_json::Value =
            serde_json::from_str(text).expect("timeout content text must be valid JSON");
        assert_eq!(
            payload["outcome"], "form_timed_out",
            "the model-facing payload must name the timeout outcome, got {payload:?}"
        );
        assert_eq!(
            payload["timeout_secs"], 1,
            "the model-facing payload must echo the resolved deadline, got {payload:?}"
        );

        // The request's own tail cleanup must deregister its bridge on every
        // resolution path (answered, cancelled, timed out) — confirm the
        // timeout path is no exception.
        assert_eq!(
            state.form_bridge_registry.bridge_count(agent_id),
            0,
            "the timed-out request's own cleanup must deregister its bridge"
        );

        // The crux of this test: a leaked suspension guard would freeze the
        // process wall-clock budget forever. Prove it clears.
        assert_eq!(
            session
                .form_suspended
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "suspension counter must return to 0 once the form resolves via timeout — \
             a stuck nonzero counter would exclude all future time on this session from \
             the process supervisor's overall-timeout budget"
        );
    }

    /// Leak guard for the in-memory `form_bridge_registry` (T2c, defect 2).
    ///
    /// Simulates the true orphan case a background reaper exists for: the
    /// request's own future is torn down (`.abort()`) without ever reaching
    /// its tail cleanup (`form_bridge.cancel_pending()` +
    /// `form_bridge_registry.deregister(...)`) — the same shape as the
    /// owning CLI subprocess dying mid-suspension and the connection
    /// carrying this request being dropped before `handle_mcp_request`
    /// returns. Asserts the registry entry is still reaped once the session
    /// ends, via the detached watcher spawned alongside the bridge in
    /// `handle_mcp_request`, not the request's own (never-reached) cleanup.
    #[tokio::test]
    async fn abandoned_sync_form_bridge_is_reaped_from_registry_when_session_ends() {
        let (state, tmp) = setup_state().await;
        // See `SYNC_FORM_TIMEOUT_ENV_LOCK`: same reasoning as the
        // cancellation reachability test above.
        let _timeout_env_guard = SYNC_FORM_TIMEOUT_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let agent_id = "agent-reap";
        let session_id = "session-reap";
        let agent = make_agent(agent_id);
        state.persistence.agents.create(&agent).await.unwrap();
        state
            .mcp_sessions
            .register_session_with_chains(
                session_id.to_string(),
                agent_id.to_string(),
                tmp.path().to_path_buf(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .expect("register mcp session");

        let mut events = state.event_bus.subscribe();

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "AskUserQuestionWithForm",
                "arguments": {
                    "title": "Reap probe",
                    "mode": "sync",
                    "questions": [{"id": "x", "type": "text", "label": "L"}]
                }
            }
        });
        let body_bytes = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());

        let call_state = Arc::clone(&state);
        let call_agent_id = agent_id.to_string();
        let call_session_id = session_id.to_string();
        let call = tokio::spawn(async move {
            handle_mcp_request(
                State(call_state),
                Path((call_agent_id, call_session_id)),
                HeaderMap::new(),
                body_bytes,
            )
            .await
        });

        wait_for_form_request(&mut events).await;
        assert_eq!(
            state.form_bridge_registry.bridge_count(agent_id),
            1,
            "the bridge must be registered while the sync form is suspended"
        );

        // Abandon the request without letting it reach its own cleanup — the
        // exact shape of a dead CLI subprocess taking its connection with it.
        call.abort();
        let _ = call.await;

        assert_eq!(
            state.form_bridge_registry.bridge_count(agent_id),
            1,
            "setup invariant: abort alone must not clean up the bridge — only \
             session teardown should"
        );

        // End the session the way real teardown does. This must reach the
        // detached reaper spawned alongside the now-abandoned bridge.
        state.mcp_sessions.remove(session_id);

        tokio::time::timeout(Duration::from_secs(5), async {
            while state.form_bridge_registry.bridge_count(agent_id) > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("form_bridge_registry entry was never reaped after session end — leak");
    }
}
