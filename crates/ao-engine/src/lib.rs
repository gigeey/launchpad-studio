pub mod agent_cascade;
pub mod agent_context;
pub mod agent_routing;
pub mod agent_watch;
pub mod artifact_regen;
pub mod artifact_task_status;
pub mod assignment_runner;
pub mod channels;
pub mod connector_event_catalog;
pub mod github_comment;
pub mod classifier_reconciler;
pub mod agent_runner;
pub mod delegate_completion;
pub mod agent_sleep_guard;
pub mod system_prompt_composer;
pub mod command_queue;
pub mod context;
pub mod context_cache;
pub mod copilot_context;
pub mod delegation_extraction;
pub mod dispatch_watchdog;
pub mod event_bus;
pub mod history;
pub mod project_context;
pub mod project_queue_manager;
pub mod agent_snapshot_sync;
pub mod instance_registry;
pub mod instructions;
pub mod mailbox_poller;
pub mod mcp_session;
pub mod memory_instructions;
pub mod plugin_auto_discovery;
pub mod plugin_cache;
pub mod plugin_catalog;
pub mod plugin_install;
pub mod plugin_manifest;
pub mod plugin_mcp;
pub mod plugin_paths;
pub mod plugin_refresh;
pub mod plugin_registry;
pub mod plugin_resolver;
pub mod plugin_uninstall;
pub mod prompt_assembler;
pub mod prompt_sections;
pub mod queue_manager;
pub mod rules;
pub mod schedule_runner;
pub use ao_engine_tools_core::skill_registry::usage as skill_usage;
pub mod skills;
pub mod sleep_guard;
pub mod state;
pub mod sync_form_reaper;
pub mod tag_stream_scanner;
pub mod task_classifier;
pub mod task_transcript_pruner;
pub mod task_comment_extraction;
pub mod task_feeder;
pub mod task_owner_extraction;
pub mod tasklist_extraction;
pub mod telegram;
pub mod tasklist_lifecycle;
pub mod tasklist_queue_manager;
pub mod tasklist_service;
pub mod tasklist_runtime;
pub mod workflow_queue_manager;
pub mod workflow_registry;
pub mod workflow_runner;
pub mod verification_cli_provider;
pub mod cli_inspection_verifier;
pub mod webhook_dispatch;
pub mod memory_promotion;
pub mod reflection_subscriber;
pub mod skill_distillation;

pub use state::AppState;
pub use ao_engine_tools_runner::prompt_bridge::LiveFormBridge;
pub use verification_cli_provider::CliProviderClient;
pub use cli_inspection_verifier::CliInspectionVerifier;

/// Resolve the completion provider client any out-of-band, single-call model
/// pass should use for a given agent profile, so that pass runs against the
/// SAME backend the agent itself runs against — only the prompt and
/// transcript differ. Shared by verification, thread summarization, and the
/// reflection pass — anything that needs "drive a model the
/// way this profile already proves in production" rather than standing up
/// its own client.
///
/// - `runner_mode = Api` routes through [`agent_runner::DefaultProviderFactory`],
///   which reads `providers.toml` and selects Anthropic vs OpenAI from the
///   profile's `native_provider`. A missing or malformed config yields `None`.
/// - `runner_mode = Cli` builds a [`CliProviderClient`] from the profile's CLI
///   configuration, so the caller shells out to the same binary the agent
///   uses instead of billing a metered API.
///
/// Returns `None` only on the Api path when no provider is configured; the
/// caller then skips injecting whatever engine it was building rather than
/// failing.
fn provider_client_for_profile(
    profile: &ao_protocol::agent::AgentProfile,
) -> Option<std::sync::Arc<dyn ao_engine_tools_runner::provider::ProviderClient>> {
    use crate::agent_runner::{DefaultProviderFactory, ProviderFactory};
    use ao_protocol::agent::AgentRunnerMode;

    match profile.runner_mode {
        AgentRunnerMode::Api => DefaultProviderFactory.build(profile).ok(),
        AgentRunnerMode::Cli => Some(std::sync::Arc::new(CliProviderClient::new(profile.clone()))
            as std::sync::Arc<dyn ao_engine_tools_runner::provider::ProviderClient>),
    }
}

/// Build a quick (single-model-call) verification engine that runs against the
/// coordinator's own provider/runner configuration. Returns `None` when no
/// provider is configured so the caller can skip injecting the engine rather
/// than failing at startup.
pub fn build_quick_verification_engine(
    profile: &ao_protocol::agent::AgentProfile,
) -> Option<std::sync::Arc<dyn ao_engine_tools_core::VerificationEngine>>
{
    let provider = provider_client_for_profile(profile)?;
    let engine = ao_engine_tools_runner::verification::ProviderVerificationEngine::new(provider);
    Some(std::sync::Arc::new(engine))
}

/// Build the summarization engine used by the `SummarizeThread` tool to
/// condense another thread's transcript into prose. Runs against the same
/// coordinator provider/runner configuration as `build_quick_verification_engine`
/// (works for both `Api` and `Cli` runner modes). Returns `None` when no
/// provider is configured so the caller can skip injecting the engine rather
/// than failing at startup.
pub fn build_thread_summarization_engine(
    profile: &ao_protocol::agent::AgentProfile,
) -> Option<std::sync::Arc<dyn ao_engine_tools_core::ThreadSummarizationEngine>>
{
    let provider = provider_client_for_profile(profile)?;
    let engine = ao_engine_tools_runner::thread_summary::ProviderThreadSummarizer::new(provider);
    Some(std::sync::Arc::new(engine))
}

/// Build the provider client the reflection pass
/// should drive for `profile` — the execution-engine seam. Thin wrapper
/// over [`provider_client_for_profile`] kept as its own `build_*` function
/// for symmetry with [`build_quick_verification_engine`] /
/// [`build_thread_summarization_engine`] and so
/// [`reflection_subscriber::ReflectionSubscriber`]'s production wiring has a
/// single, discoverable call site rather than reaching for the shared
/// private helper directly. Returns `None` when no provider is configured
/// for `profile`, so the caller can skip this reflection pass run rather
/// than failing.
pub fn build_reflection_provider(
    profile: &ao_protocol::agent::AgentProfile,
) -> Option<std::sync::Arc<dyn ao_engine_tools_runner::provider::ProviderClient>> {
    provider_client_for_profile(profile)
}

/// Build the provider client the `POST /prompt-refine` route should drive
/// for `profile` — the assignment's owning agent, so a refine call runs
/// against the same provider/model that agent's real runs use. Thin
/// wrapper over [`provider_client_for_profile`] kept as its own `build_*`
/// function for symmetry with [`build_reflection_provider`] /
/// [`build_quick_verification_engine`] /
/// [`build_thread_summarization_engine`]. Returns `None` when no provider
/// is configured for `profile`, so the caller can surface a clear
/// "add an API key" error instead of failing opaquely.
pub fn build_prompt_refine_provider(
    profile: &ao_protocol::agent::AgentProfile,
) -> Option<std::sync::Arc<dyn ao_engine_tools_runner::provider::ProviderClient>> {
    provider_client_for_profile(profile)
}

/// Build a full (inspection) verification engine that runs against the
/// coordinator's own provider/runner configuration and the supplied tool
/// registry.
///
/// For `runner_mode = Api`, this spawns an isolated native child session
/// (filtered registry: Read, Glob, Grep, Bash) driven by our query loop.
/// For `runner_mode = Cli`, this invokes the coordinator's CLI binary
/// directly with the project's working directory as CWD, avoiding the
/// native query-loop seam that requires a tool-calling API provider.
///
/// Returns `None` when no provider is configured (Api path only — the Cli
/// path always yields an engine since the binary configuration comes from
/// the profile, not from `providers.toml`).
pub fn build_full_verification_engine(
    profile: &ao_protocol::agent::AgentProfile,
    registry: std::sync::Arc<ao_engine_tools_core::Registry>,
) -> Option<std::sync::Arc<dyn ao_engine_tools_core::VerificationEngine>>
{
    use ao_protocol::agent::AgentRunnerMode;

    match profile.runner_mode {
        AgentRunnerMode::Cli => {
            let engine = CliInspectionVerifier::new(profile.clone());
            Some(std::sync::Arc::new(engine))
        }
        AgentRunnerMode::Api => {
            let provider = provider_client_for_profile(profile)?;
            let engine = ao_engine_tools_runner::verification::InspectionVerifier::new(
                provider,
                ao_engine_tools_runner::hooks::config::RunnerSettings::default(),
                registry,
            );
            Some(std::sync::Arc::new(engine))
        }
    }
}

#[cfg(test)]
mod tests;
