pub mod agent_commands;
pub mod agent_tasklists;
pub mod agents;
pub mod artifacts;
pub mod assignments;
pub mod attachments;
pub mod form_answers;
pub mod mcp;
pub mod mcp_servers;
pub mod bookmarks;
pub mod channels;
pub mod context;
pub mod delegates;
pub mod instructions;
pub mod memories;
pub mod messages;
pub mod phase_attachments;
pub mod preferences;
pub mod project_attachments;
pub mod project_messages;
pub mod projects;
pub mod prompt_refine;
pub mod providers;
pub mod rules;
pub mod sessions;
pub mod search;
pub mod skills;
pub mod stream;
pub mod system;
pub mod telegram;
pub mod threads;
pub mod webhooks;
pub mod workflows;
pub mod workspaces;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use ao_engine::AppState;

/// Backstop bound on `GET /providers/{name}/models` at the axum layer, on
/// top of the discovery client's own connect/request timeouts
/// (`ao_engine_tools_provider_config::model_discovery::{CONNECT_TIMEOUT,
/// REQUEST_TIMEOUT}`). Those client-side timeouts are what actually bounds
/// the outbound call in practice — this exists purely so the route can never
/// hang the connection open indefinitely if that assumption is ever broken
/// by a future change, so it's set well above the client's own ceiling
/// rather than to fire in the common case. Scoped to just this one route via
/// a merged sub-router (below) rather than applied to the whole router: this
/// crate also serves several long-lived SSE streams (`stream::stream_*`)
/// that a blanket timeout would kill.
const PROVIDER_MODELS_ROUTE_TIMEOUT: Duration = Duration::from_secs(20);

/// Build the Axum router with all routes, CORS, and tracing middleware.
pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::permissive();

    // `GET /providers/{name}/models` gets its own timeout-layered
    // sub-router, merged below, instead of a plain `.route(...)` entry —
    // see [`PROVIDER_MODELS_ROUTE_TIMEOUT`] for why this can't just be a
    // `.layer()` on the router returned from this function.
    let provider_models_route = Router::new()
        .route(
            "/providers/{name}/models",
            axum::routing::get(providers::list_provider_models),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            PROVIDER_MODELS_ROUTE_TIMEOUT,
        ));

    Router::new()
        .route("/agents", axum::routing::post(agents::create_agent))
        .route("/agents", axum::routing::get(agents::list_agents))
        .route("/agents/{id}/compose-prompt", axum::routing::get(agents::compose_prompt))
        .route("/agents/{id}", axum::routing::get(agents::get_agent))
        .route("/agents/{id}", axum::routing::put(agents::update_agent))
        .route("/agents/{id}", axum::routing::delete(agents::delete_agent))
        .route(
            "/agents/{parent_id}/clone",
            axum::routing::post(agents::clone_agent),
        )
        .route(
            "/agents/{agent_id}/messages",
            axum::routing::post(messages::send_message),
        )
        .route(
            "/agents/{agent_id}/messages",
            axum::routing::get(messages::get_messages),
        )
        .route(
            "/agents/{agent_id}/precompute-context",
            axum::routing::post(context::precompute_context),
        )
        .route(
            "/agents/{agent_id}/skills",
            axum::routing::post(skills::write_skill).get(skills::list_skills),
        )
        .route(
            "/agents/{agent_id}/skills/refresh",
            axum::routing::post(skills::refresh_skills),
        )
        .route(
            "/agents/{agent_id}/skills/import-folder",
            axum::routing::post(skills::import_folder),
        )
        .route(
            "/agents/{agent_id}/skills/import-file",
            axum::routing::post(skills::import_file),
        )
        .route(
            "/agents/{agent_id}/skills/{skill_id}",
            axum::routing::delete(skills::delete_skill).patch(skills::patch_skill),
        )
        .route(
            "/agents/{agent_id}/skills/review",
            axum::routing::get(skills::list_skill_review_queue),
        )
        .route(
            "/agents/{agent_id}/skills/review/promote",
            axum::routing::post(skills::promote_skill_observation),
        )
        .route(
            "/agents/{agent_id}/skills/review/{skill_name}",
            axum::routing::post(skills::act_on_skill_review_candidate),
        )
        .route(
            "/skills/launchpad/global",
            axum::routing::get(skills::list_launchpad_global_skills),
        )
        .route(
            "/skills/launchpad/project",
            axum::routing::get(skills::list_launchpad_project_skills),
        )
        .route(
            "/skills/launchpad/promote",
            axum::routing::post(skills::promote_launchpad_skill),
        )
        .route(
            "/agents/{agent_id}/launchpad-skills/global",
            axum::routing::post(skills::set_launchpad_global_skill_enabled),
        )
        .route(
            "/agents/{agent_id}/launchpad-skills/project",
            axum::routing::post(skills::set_launchpad_project_skill_enabled),
        )
        .route(
            "/agents/{agent_id}/rules",
            axum::routing::get(rules::list_rules),
        )
        .route(
            "/agents/{agent_id}/rules/refresh",
            axum::routing::post(rules::refresh_rules),
        )
        .route(
            "/agents/{agent_id}/rules/import-file",
            axum::routing::post(rules::import_file),
        )
        .route(
            "/agents/{agent_id}/rules/import-folder",
            axum::routing::post(rules::import_folder),
        )
        .route(
            "/agents/{agent_id}/rules/import-link",
            axum::routing::post(rules::import_link),
        )
        .route(
            "/agents/{agent_id}/rules/{*rule_id}",
            axum::routing::delete(rules::delete_rule).patch(rules::patch_rule),
        )
        .route(
            "/agents/{agent_id}/instructions",
            axum::routing::get(instructions::list_instructions),
        )
        .route(
            "/agents/{agent_id}/instructions/{id}",
            axum::routing::patch(instructions::patch_instruction),
        )
        .route(
            "/agents/{agent_id}/form-answer",
            axum::routing::post(form_answers::submit_form_answer),
        )
        .route(
            "/agents/{agent_id}/async-forms/{form_id}/answer",
            axum::routing::post(form_answers::async_form_answer),
        )
        .route(
            "/agents/{agent_id}/async-forms/{form_id}/dismiss",
            axum::routing::post(form_answers::async_form_dismiss),
        )
        .route(
            "/agents/{agent_id}/cancel",
            axum::routing::post(messages::cancel_agent_run),
        )
        .route(
            "/agents/{agent_id}/stream",
            axum::routing::get(stream::stream_events),
        )
        .route(
            "/agents/{agent_id}/delegates",
            axum::routing::get(delegates::list_agent_delegates),
        )
        .route(
            "/delegates/{delegation_id}/cancel",
            axum::routing::post(delegates::cancel_delegate),
        )
        .route(
            "/agents/{agent_id}/bookmarks",
            axum::routing::get(bookmarks::list_agent_bookmarks),
        )
        .route(
            "/agents/{agent_id}/bookmarks",
            axum::routing::post(bookmarks::add_agent_bookmark),
        )
        .route(
            "/agents/{agent_id}/bookmarks/{bookmark_id}",
            axum::routing::delete(bookmarks::delete_agent_bookmark),
        )
        .route(
            "/agents/{agent_id}/telegram/token",
            axum::routing::put(telegram::set_telegram_token)
                .delete(telegram::delete_telegram_token),
        )
        .route(
            "/agents/{agent_id}/telegram/status",
            axum::routing::get(telegram::get_telegram_status),
        )
        .route(
            "/agents/{agent_id}/telegram/pairing-code",
            axum::routing::post(telegram::create_pairing_code),
        )
        .route(
            "/agents/{agent_id}/telegram/chats/{chat_id}",
            axum::routing::delete(telegram::delete_telegram_chat),
        )
        .route(
            "/agents/{agent_id}/channels",
            axum::routing::get(channels::list_channels),
        )
        .route(
            "/agents/{agent_id}/channels/{binding_id}/senders",
            axum::routing::get(channels::get_channel_senders).put(channels::set_channel_senders),
        )
        .route(
            "/agents/{agent_id}/channels/email",
            axum::routing::put(channels::upsert_email_channel)
                .delete(channels::delete_email_channel),
        )
        .route(
            "/agents/{agent_id}/channels/email/secret",
            axum::routing::put(channels::set_email_channel_secret),
        )
        .route(
            "/agents/{agent_id}/channels/discord",
            axum::routing::put(channels::upsert_discord_channel)
                .delete(channels::delete_discord_channel),
        )
        .route(
            "/agents/{agent_id}/channels/discord/secret",
            axum::routing::put(channels::set_discord_channel_secret),
        )
        .route(
            "/agents/{agent_id}/channels/slack",
            axum::routing::put(channels::upsert_slack_channel)
                .delete(channels::delete_slack_channel),
        )
        .route(
            "/agents/{agent_id}/channels/slack/secret",
            axum::routing::put(channels::set_slack_channel_secret),
        )
        .route(
            "/agents/{agent_id}/channels/slack/manifest",
            axum::routing::get(channels::get_slack_manifest),
        )
        .route(
            "/agents/{agent_id}/channels/slack/test-connection",
            axum::routing::post(channels::test_slack_connection),
        )
        .route(
            "/agents/memories/summary",
            axum::routing::get(memories::get_agent_memory_summaries),
        )
        .route(
            "/agents/{agent_id}/memories",
            axum::routing::get(memories::list_agent_memories),
        )
        .route(
            "/agents/{agent_id}/memories",
            axum::routing::post(memories::add_agent_memory),
        )
        .route(
            "/agents/{agent_id}/memories/{memory_id}",
            axum::routing::delete(memories::delete_agent_memory),
        )
        .route(
            "/memories/global",
            axum::routing::get(memories::list_global_memories),
        )
        .route(
            "/memories/global",
            axum::routing::post(memories::add_global_memory),
        )
        .route(
            "/memories/global/{memory_id}",
            axum::routing::delete(memories::delete_global_memory),
        )
        .route(
            "/agents/{agent_id}/memories/project",
            axum::routing::get(memories::list_project_memories),
        )
        .route(
            "/agents/{agent_id}/memories/project",
            axum::routing::post(memories::add_project_memory),
        )
        .route(
            "/agents/{agent_id}/memories/project/{memory_id}",
            axum::routing::delete(memories::delete_project_memory),
        )
        .route(
            "/memories/thread/{thread_id}",
            axum::routing::get(memories::list_thread_memories),
        )
        .route(
            "/memories/thread/{thread_id}",
            axum::routing::post(memories::add_thread_memory),
        )
        .route(
            "/memories/thread/{thread_id}/{memory_id}",
            axum::routing::delete(memories::delete_thread_memory),
        )
        .route(
            "/agents/{agent_id}/memories/review",
            axum::routing::get(memories::list_review_queue),
        )
        .route(
            "/agents/{agent_id}/memories/review/{candidate_id}",
            axum::routing::post(memories::act_on_review_candidate),
        )
        .route(
            "/agents/{agent_id}/memories/undo",
            axum::routing::post(memories::undo_memory_write),
        )
        .route(
            "/agents/{agent_id}/tasklists",
            axum::routing::post(agent_tasklists::create_tasklist)
                .get(agent_tasklists::list_tasklists),
        )
        .route(
            "/agents/{agent_id}/tasklists/{tasklist_id}",
            axum::routing::get(agent_tasklists::get_tasklist),
        )
        .route(
            "/agents/{agent_id}/tasklists/{tasklist_id}/tasks",
            axum::routing::post(agent_tasklists::append_task),
        )
        .route(
            "/agents/{agent_id}/tasklists/{tasklist_id}/status",
            axum::routing::post(agent_tasklists::set_tasklist_status),
        )
        .route(
            "/agents/{agent_id}/tasklists/{tasklist_id}/tasks/{task_id}/skip",
            axum::routing::post(agent_tasklists::skip_task),
        )
        .route(
            "/agents/{agent_id}/tasklists/{tasklist_id}/stream",
            axum::routing::get(stream::stream_agent_tasklist_events),
        )
        .route("/projects", axum::routing::post(projects::create_project))
        .route("/projects", axum::routing::get(projects::list_projects))
        .route("/projects/{id}", axum::routing::get(projects::get_project))
        .route("/projects/{id}", axum::routing::patch(projects::patch_project))
        .route("/projects/{id}", axum::routing::delete(projects::delete_project))
        .route(
            "/projects/{id}/tasklists",
            axum::routing::get(projects::list_project_tasklists),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}",
            axum::routing::get(projects::get_project_tasklist),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/tasks",
            axum::routing::post(projects::append_project_task),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/status",
            axum::routing::post(projects::set_project_tasklist_status),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/continue",
            axum::routing::post(projects::continue_project_tasklist),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/discard",
            axum::routing::post(projects::discard_project_tasklist),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/replay",
            axum::routing::post(projects::replay_project_tasklist),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/tasks/{task_id}/skip",
            axum::routing::post(projects::skip_project_task),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/tasks/{task_id}/stop",
            axum::routing::post(projects::stop_project_task),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/tasks/{task_id}/resume",
            axum::routing::post(projects::resume_project_task),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/tasks/{task_id}/comments",
            axum::routing::post(projects::add_project_task_comment),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/outputs/{*filename}",
            axum::routing::get(projects::get_project_tasklist_output),
        )
        .route(
            "/projects/{id}/tasklists/{tasklist_id}/copilot",
            axum::routing::get(projects::get_project_copilot),
        )
        .route(
            "/projects/{id}/messages",
            axum::routing::post(project_messages::send_project_message),
        )
        .route(
            "/projects/{id}/messages",
            axum::routing::get(project_messages::get_project_messages),
        )
        .route(
            "/projects/{id}/cancel",
            axum::routing::post(project_messages::cancel_project_run),
        )
        .route(
            "/projects/{id}/attachments",
            axum::routing::post(project_attachments::upload_project_attachment),
        )
        .route(
            "/projects/{id}/attachments/folder",
            axum::routing::post(project_attachments::upload_project_folder_reference),
        )
        .route(
            "/projects/{id}/attachments/{attachment_id}",
            axum::routing::get(project_attachments::serve_project_attachment),
        )
        .route(
            "/projects/{id}/attachments/{attachment_id}",
            axum::routing::delete(project_attachments::delete_project_attachment),
        )
        .route(
            "/projects/{id}/attachments/{attachment_id}/info",
            axum::routing::get(project_attachments::get_project_attachment_info),
        )
        .route(
            "/projects/{id}/stream",
            axum::routing::get(stream::stream_project_events),
        )
        .route(
            "/projects/{id}/form-answer",
            axum::routing::post(form_answers::submit_form_answer_project),
        )
        .route(
            "/projects/{project_id}/async-forms/{form_id}/answer",
            axum::routing::post(form_answers::async_form_answer_project),
        )
        .route(
            "/projects/{project_id}/async-forms/{form_id}/dismiss",
            axum::routing::post(form_answers::async_form_dismiss_project),
        )
        .route(
            "/preferences",
            axum::routing::get(preferences::get_preferences),
        )
        .route(
            "/preferences",
            axum::routing::put(preferences::put_preferences),
        )
        .route(
            "/preferences/status",
            axum::routing::get(preferences::get_preferences_status),
        )
        .route(
            "/preferences/instruction-filenames",
            axum::routing::get(preferences::get_instruction_filenames)
                .put(preferences::put_instruction_filenames),
        )
        .route(
            "/workspaces",
            axum::routing::get(workspaces::list_workspaces).post(workspaces::create_workspace),
        )
        .route(
            "/workspaces/active",
            axum::routing::get(workspaces::get_active_workspace),
        )
        .route(
            "/workspaces/{id}",
            axum::routing::patch(workspaces::rename_workspace).delete(workspaces::delete_workspace),
        )
        .route(
            "/workspaces/{id}/activate",
            axum::routing::post(workspaces::activate_workspace),
        )
        .route(
            "/workspaces/{id}/duplicate",
            axum::routing::post(workspaces::duplicate_workspace),
        )
        .route(
            "/agents/{agent_id}/attachments",
            axum::routing::post(attachments::upload_attachment),
        )
        .route(
            "/agents/{agent_id}/attachments",
            axum::routing::get(attachments::list_attachments),
        )
        .route(
            "/agents/{agent_id}/attachments/folder",
            axum::routing::post(attachments::upload_folder_reference),
        )
        .route(
            "/agents/{agent_id}/attachments/{attachment_id}",
            axum::routing::get(attachments::serve_attachment),
        )
        .route(
            "/agents/{agent_id}/attachments/{attachment_id}",
            axum::routing::delete(attachments::delete_attachment),
        )
        .route(
            "/agents/{agent_id}/attachments/{attachment_id}/info",
            axum::routing::get(attachments::get_attachment_info),
        )
        .route(
            "/agents/{agent_id}/artifacts",
            axum::routing::post(artifacts::create_artifact),
        )
        .route(
            "/agents/{agent_id}/artifacts",
            axum::routing::get(artifacts::list_artifacts),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}",
            axum::routing::get(artifacts::get_artifact),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}",
            axum::routing::delete(artifacts::delete_artifact),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}/refresh",
            axum::routing::put(artifacts::refresh_artifact),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}/undo",
            axum::routing::post(artifacts::undo_artifact),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}/regenerate",
            axum::routing::post(artifacts::regenerate_artifact),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}/chat",
            axum::routing::post(artifacts::chat_artifact),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}/chat",
            axum::routing::get(artifacts::get_artifact_chat),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}/task/{task_id}/status",
            axum::routing::get(artifacts::get_artifact_task_status),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}/pin",
            axum::routing::put(artifacts::set_artifact_pinned),
        )
        .route(
            "/agents/{agent_id}/artifacts/{artifact_id}/group",
            axum::routing::put(artifacts::set_artifact_group),
        )
        .route(
            "/artifacts/pinned",
            axum::routing::get(artifacts::list_pinned_artifacts),
        )
        .route(
            "/artifact-groups",
            axum::routing::post(artifacts::create_artifact_group),
        )
        .route(
            "/artifact-groups",
            axum::routing::get(artifacts::list_artifact_groups),
        )
        .route(
            "/artifact-groups/{group_id}",
            axum::routing::delete(artifacts::delete_artifact_group),
        )
        .route("/system/config", axum::routing::get(system::get_config))
        .route("/system/storage", axum::routing::get(system::get_storage))
        .route(
            "/system/cleanup",
            axum::routing::post(system::trigger_cleanup),
        )
        .route("/system/logs", axum::routing::get(system::get_logs))
        .route(
            "/system/logs/clear",
            axum::routing::post(system::clear_logs),
        )
        .route(
            "/workflows",
            axum::routing::get(workflows::list_workflows),
        )
        .route(
            "/workflows/refresh",
            axum::routing::post(workflows::refresh_workflows),
        )
        .route(
            "/workflows/import",
            axum::routing::post(workflows::import_workflow),
        )
        .route(
            "/workflows/clone-example",
            axum::routing::post(workflows::clone_example),
        )
        .route(
            "/workflows/{id}",
            axum::routing::get(workflows::get_workflow),
        )
        .route(
            "/workflows/{id}/tasks",
            axum::routing::post(workflows::create_task),
        )
        .route("/tasks", axum::routing::get(workflows::list_tasks))
        .route(
            "/tasks/{id}",
            axum::routing::get(workflows::get_task)
                .delete(workflows::delete_task),
        )
        .route(
            "/tasks/{id}/archive",
            axum::routing::post(workflows::archive_task),
        )
        .route(
            "/tasks/{id}/output/{filename}",
            axum::routing::get(workflows::get_task_output),
        )
        .route(
            "/tasks/{id}/phases/{phase}/complete",
            axum::routing::post(workflows::complete_phase),
        )
        .route(
            "/tasks/{id}/start",
            axum::routing::post(workflows::start_task),
        )
        .route(
            "/tasks/{id}/resume",
            axum::routing::post(workflows::resume_task),
        )
        .route(
            "/tasks/{id}/cancel",
            axum::routing::post(workflows::cancel_task),
        )
        .route(
            "/tasks/{id}/phases/{phase}/messages",
            axum::routing::get(workflows::get_phase_messages),
        )
        .route(
            "/tasks/{id}/phases/{phase}/messages",
            axum::routing::post(workflows::send_phase_message),
        )
        .route(
            "/tasks/{id}/phases/{phase}/start",
            axum::routing::post(workflows::start_phase_agent),
        )
        .route(
            "/tasks/{id}/phases/{phase}/attachments",
            axum::routing::post(phase_attachments::upload_phase_attachment),
        )
        .route(
            "/tasks/{id}/phases/{phase}/attachments",
            axum::routing::get(phase_attachments::list_phase_attachments),
        )
        .route(
            "/tasks/{id}/phases/{phase}/attachments/folder",
            axum::routing::post(phase_attachments::upload_phase_folder_reference),
        )
        .route(
            "/tasks/{id}/phases/{phase}/attachments/{attachment_id}",
            axum::routing::get(phase_attachments::serve_phase_attachment),
        )
        .route(
            "/tasks/{id}/phases/{phase}/attachments/{attachment_id}",
            axum::routing::delete(phase_attachments::delete_phase_attachment),
        )
        .route(
            "/tasks/{id}/phases/{phase}/attachments/{attachment_id}/info",
            axum::routing::get(phase_attachments::get_phase_attachment_info),
        )
        .route(
            "/tasks/{id}/phases/{phase}/submit-input",
            axum::routing::post(workflows::submit_input),
        )
        .route(
            "/tasks/{id}/stream",
            axum::routing::get(stream::stream_task_events),
        )
        .route(
            "/agents/commands",
            axum::routing::get(agent_commands::list_agent_commands),
        )
        .route(
            "/agents/{agent_id}/threads",
            axum::routing::get(threads::list_agent_threads)
                .post(threads::create_agent_thread),
        )
        .route(
            "/threads",
            axum::routing::get(threads::list_all_threads),
        )
        .route(
            "/threads/{thread_id}",
            axum::routing::get(threads::get_thread)
                .patch(threads::patch_thread)
                .delete(threads::delete_thread),
        )
        .route(
            "/threads/{thread_id}/archive",
            axum::routing::post(threads::archive_thread),
        )
        .route(
            "/threads/{thread_id}/unarchive",
            axum::routing::post(threads::unarchive_thread),
        )
        .route(
            "/agents/{agent_id}/assignments",
            axum::routing::get(assignments::list_agent_assignments)
                .post(assignments::create_assignment),
        )
        .route(
            "/assignments/{assignment_id}",
            axum::routing::get(assignments::get_assignment)
                .patch(assignments::patch_assignment)
                .delete(assignments::delete_assignment),
        )
        .route(
            "/assignments/{assignment_id}/runs",
            axum::routing::get(assignments::list_assignment_runs),
        )
        .route(
            "/assignments/{assignment_id}/trigger",
            axum::routing::post(assignments::trigger_assignment),
        )
        .route(
            "/webhooks/{route_name}",
            axum::routing::post(webhooks::handle_webhook),
        )
        .route(
            "/webhooks/{route_name}/secret",
            axum::routing::get(webhooks::get_webhook_route_secret_status).put(webhooks::set_webhook_route_secret),
        )
        .route(
            "/webhook-test",
            axum::routing::post(webhooks::test_webhook_route),
        )
        .route(
            "/prompt-refine",
            axum::routing::post(prompt_refine::refine_prompt_template),
        )
        .route("/search", axum::routing::get(search::search))
        .route(
            "/system/stream",
            axum::routing::get(stream::stream_system_events),
        )
        .route("/health", axum::routing::get(health))
        .route("/sessions", axum::routing::post(sessions::register_session))
        .route(
            "/sessions/{session_id}",
            axum::routing::delete(sessions::deregister_session),
        )
        .route(
            "/mcp-servers",
            axum::routing::get(mcp_servers::list_servers).post(mcp_servers::add_server),
        )
        .route(
            "/mcp-servers/{name}",
            axum::routing::delete(mcp_servers::delete_server),
        )
        .route(
            "/mcp-servers/{name}/authorize",
            axum::routing::post(mcp_servers::authorize_server),
        )
        .route(
            "/providers",
            axum::routing::get(providers::list_providers),
        )
        .route(
            "/providers/{name}",
            axum::routing::put(providers::set_provider).delete(providers::delete_provider),
        )
        .merge(provider_models_route)
        .route("/mcp/{agent_id}/{session_id}", axum::routing::post(mcp::handle_mcp_request))
        .with_state(state)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
}

async fn health() -> &'static str {
    "ok"
}

/// Shared test-only lock for the process-wide `LAUNCHPAD_STUDIO_DATA_DIR` env
/// var. The `ao-server` lib test binary runs every `#[cfg(test)]` module in
/// this crate concurrently on multiple threads within a single process, so
/// any test that points that var at its own temp dir must hold this one lock
/// for the full mutate-then-read window. A per-module mutex does not
/// serialize against tests in *other* modules that also flip this same var,
/// which lets two tests race and resolve each other's temp root. This is the
/// one and only lock for this var in this crate — route test modules must
/// use it instead of declaring their own.
#[cfg(test)]
pub(crate) mod env_lock {
    use std::sync::Mutex;

    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());
}
