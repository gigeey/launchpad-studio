use super::{build_thread_notes_section, compose_system_prompt, with_tool_catalog};
use ao_protocol::agent::{
    AgentProfile, AgentRunnerMode, CliProviderConfig, DelegateTarget, InputMode, OutputFormat,
    ProviderConfig,
};
use ao_protocol::memory::{MemoryEntry, MemoryScope, MemorySource};
use ao_protocol::preferences::UserPreferences;
use ao_protocol::system_prompt_context::{AgentHomeContext, WorkspaceContext};
use ao_protocol::workflow::{WorkflowSource, WorkflowSummary};
use std::collections::HashMap;

const FROZEN_DATE: &str = "2026-01-15";
const FROZEN_CWD: &str = "/frozen/workspace";

fn minimal_profile() -> AgentProfile {
    AgentProfile {
        id: "test-agent".into(),
        name: "TestBot".into(),
        description: "".into(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "claude".into(),
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
        model: Some("claude-sonnet-4-6".into()),
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
        runner_mode: AgentRunnerMode::Cli,
        enabled_plugins: HashMap::new(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
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
        max_turns: None,
    }
}

fn full_profile() -> AgentProfile {
    AgentProfile {
        description: "A helpful assistant for developers.".into(),
        model: Some("claude-opus-4-7".into()),
        persona: Some("You are a senior software engineer with 20 years of experience in distributed systems.".into()),
        special_instructions: Some("Always write tests. Never skip documentation. Prefer explicit over implicit.".into()),
        ..minimal_profile()
    }
}

fn empty_contexts() -> (WorkspaceContext, AgentHomeContext) {
    (
        WorkspaceContext {
            root_path: FROZEN_CWD.into(),
            claude_md_content: None,
            rules: vec![],
        },
        AgentHomeContext {
            claude_md_content: None,
            rules: vec![],
            skills: vec![],
            skills_block: None,
        },
    )
}

fn no_name_prefs() -> UserPreferences {
    UserPreferences {
        preferred_name: None,
        full_name: None,
        ..UserPreferences::default()
    }
}

fn make_memory(content: &str) -> MemoryEntry {
    let created_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    MemoryEntry {
        id: "mem-1".into(),
        content: content.into(),
        created_at,
        source: Some(MemorySource::Agent),
        scope: MemoryScope::Agent,
        scope_key: None,
        updated_at: created_at,
        deleted_at: None,
        confidence: 1.0,
        status: Default::default(),
        superseded_by: None,
        pinned: false,
        decay_score: 1.0,
    }
}

fn make_workflow(id: &str, name: &str, description: &str) -> WorkflowSummary {
    WorkflowSummary {
        id: id.into(),
        name: name.into(),
        version: None,
        description: Some(description.into()),
        phase_count: 3,
        source: WorkflowSource::default(),
        updated_on: None,
        last_run: None,
    }
}

fn make_delegate(id: &str, name: &str, purpose: &str, fork: bool) -> DelegateTarget {
    DelegateTarget {
        target_agent_id: id.into(),
        name: name.into(),
        purpose: purpose.into(),
        share_context_allowed: fork,
    }
}

// ── Scenario: minimal profile (name only, no description, persona, special_instructions) ──

#[test]
fn snapshot_minimal_profile() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );
    insta::assert_snapshot!(result);
}

// ── Scenario: full profile (all fields populated) ──

#[test]
fn snapshot_full_profile() {
    let profile = full_profile();
    let user_prefs = UserPreferences {
        preferred_name: Some("Alice".into()),
        full_name: Some("Alice Smith".into()),
        ..UserPreferences::default()
    };
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &user_prefs,
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );
    insta::assert_snapshot!(result);
}

// ── Scenario: empty memories — block must be absent ──

#[test]
fn snapshot_empty_memories_absent() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );
    assert!(
        !result.contains("[Agent Memories]"),
        "memory block must be absent when memories slice is empty"
    );
    insta::assert_snapshot!(result);
}

// ── Scenario: non-empty memories — block must be present ──

#[test]
fn snapshot_nonempty_memories_present() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let memories = vec![
        make_memory("Prefer verbose output for debugging sessions."),
        make_memory("User prefers camelCase for variable names."),
    ];
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &memories,
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );
    assert!(
        result.contains("[Agent Memories]"),
        "memory block must be present when memories slice is non-empty"
    );
    insta::assert_snapshot!(result);
}

// ── Scenario: thread notes — absent when there are no thread entries ──

#[test]
fn thread_notes_absent_when_empty() {
    assert_eq!(
        build_thread_notes_section(&[]),
        None,
        "no active thread (or a thread with no entries) must inject nothing"
    );
}

// ── Scenario: thread notes — present and distinct from Section 11 memories ──

#[test]
fn thread_notes_present_and_delimited() {
    let entries = vec![make_memory("Remember: user wants the terse variant this thread.")];
    let block = build_thread_notes_section(&entries).expect("block must be present");
    assert!(
        block.starts_with("[Thread Notes]"),
        "thread notes must be its own clearly-labeled block: {block}"
    );
    assert!(block.contains("Remember: user wants the terse variant this thread."));
}

// ── Scenario: no workflows — block must be absent ──

#[test]
fn snapshot_no_workflows_absent() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );
    assert!(
        !result.contains("# Workflows"),
        "workflows block must be absent when workflows slice is empty"
    );
    insta::assert_snapshot!(result);
}

// ── Scenario: multiple workflows — only id and name, not full descriptions ──

#[test]
fn snapshot_multiple_workflows_id_and_name_only() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let workflows = vec![
        make_workflow(
            "wf-deploy",
            "Deploy to Production",
            "Full deployment pipeline with smoke tests and staged rollback procedures",
        ),
        make_workflow(
            "wf-review",
            "Code Review",
            "Automated style checks, lint, and PR feedback generation",
        ),
    ];
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &workflows,
        &[],
        FROZEN_DATE,
        None,
    );
    assert!(result.contains("wf-deploy"), "workflow id must appear");
    assert!(result.contains("Deploy to Production"), "workflow name must appear");
    assert!(
        !result.contains("smoke tests and staged rollback"),
        "workflow description must NOT appear"
    );
    insta::assert_snapshot!(result);
}

// ── Scenario: no delegate targets — block must be absent ──

#[test]
fn snapshot_no_delegate_targets_absent() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );
    assert!(
        !result.contains("# Delegate Targets"),
        "delegate block must be absent when targets slice is empty"
    );
    insta::assert_snapshot!(result);
}

// ── Scenario: multiple delegate targets ──

#[test]
fn snapshot_multiple_delegate_targets() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let targets = vec![
        make_delegate("agent-writer", "Writer", "Drafts and edits documentation.", true),
        make_delegate("agent-reviewer", "Reviewer", "Reviews code for correctness.", false),
    ];
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &targets,
        FROZEN_DATE,
        None,
    );
    assert!(result.contains("# Delegate Targets"), "delegate block must be present");
    assert!(result.contains("Writer"), "first delegate name must appear");
    assert!(result.contains("Reviewer"), "second delegate name must appear");
    insta::assert_snapshot!(result);
}

// ── Scenario: delegate targets appear immediately after BASELINE_GUIDANCE (before workflows) ──

#[test]
fn snapshot_delegate_targets_after_baseline() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let targets = vec![make_delegate(
        "agent-writer",
        "Writer",
        "Drafts and edits documentation.",
        true,
    )];
    let workflows = vec![make_workflow("wf-1", "Some Workflow", "A workflow description")];
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &workflows,
        &targets,
        FROZEN_DATE,
        None,
    );
    let delegate_pos = result.find("# Delegate Targets").unwrap();
    let workflows_pos = result.find("# Workflows").unwrap();
    assert!(
        delegate_pos < workflows_pos,
        "# Delegate Targets must appear before # Workflows (delegate is now adjacent to BASELINE_GUIDANCE)"
    );
    insta::assert_snapshot!(result);
}

// ── Parity test: CLI output == native body + catalog suffix ──

#[test]
fn cli_and_native_outputs_match_modulo_catalog() {
    let profile = full_profile();
    let user_prefs = UserPreferences {
        preferred_name: Some("Alice".into()),
        full_name: Some("Alice Smith".into()),
        ..UserPreferences::default()
    };
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let memories = vec![make_memory("Use async/await patterns for I/O-bound work.")];
    let workflows = vec![make_workflow(
        "wf-deploy",
        "Deploy to Production",
        "This description should NOT appear in the prompt output",
    )];
    let targets = vec![make_delegate(
        "agent-writer",
        "Writer",
        "Drafts documentation.",
        false,
    )];

    let canonical = compose_system_prompt(
        &profile,
        &user_prefs,
        &workspace_ctx,
        &agent_home_ctx,
        &memories,
        &[],
        &[],
        &workflows,
        &targets,
        FROZEN_DATE,
        None,
    );

    let catalog_xml =
        "<tools><tool name=\"Read\"><description>Read files</description></tool></tools>";
    let cli_output = with_tool_catalog(canonical.clone(), catalog_xml);

    let catalog_divider = "\n\n# Tool calls\n\n";
    let (cli_body, _) = cli_output
        .split_once(catalog_divider)
        .expect("CLI output must contain the tool catalog divider");

    assert_eq!(
        canonical, cli_body,
        "CLI canonical body must be byte-for-byte identical to native body"
    );
}

// ── Scenario: project_key present — emitted inside <run-context> ──

#[test]
fn snapshot_with_project_key() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        Some("/frozen/workspace"),
    );
    assert!(
        result.contains("<project-key>/frozen/workspace</project-key>"),
        "project-key element must appear inside run-context when Some"
    );
    insta::assert_snapshot!(result);
}

// ── Scenario: project_key None — element must be absent ──

#[test]
fn project_key_absent_when_none() {
    let profile = minimal_profile();
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );
    assert!(
        !result.contains("<project-key>"),
        "project-key element must be absent when project_key is None"
    );
}

// ── Scenario: registry-derived skills_block reaches the composed prompt ──
//
// Regression: a profile's enabled plugin skill must appear in the composed
// system prompt. Previously the listing was driven by the agent-home
// `skills/` directory alone (empty for pool/plugin agents), so the model was
// never told its enabled skills existed even though dispatch could resolve
// them. The runner now renders the registry into `skills_block`;
// this test exercises that block flowing through `compose_system_prompt`.

#[test]
fn skills_block_renders_into_composed_prompt() {
    use crate::agent_context::render_studio_skills_block;
    use ao_engine_tools_core::skill_registry::{
        ContextMode, SkillEntry, SkillRecord, SkillRegistry, SkillSource,
    };

    let mut registry = SkillRegistry::empty();
    registry.insert(
        "brainstorming".to_string(),
        SkillEntry::Ok(SkillRecord {
            name: "brainstorming".to_string(),
            description: "Structured idea generation".to_string(),
            context: ContextMode::Inline,
            agent: None,
            allowed_tools: vec![],
            arguments: vec![],
            body: "body".to_string(),
            source: SkillSource::Plugin {
                plugin_name: "superpowers".to_string(),
            },
            when_to_use: None,
            model: None,
            disable_model_invocation: false,
            provenance: ao_engine_tools_core::skill_registry::SkillProvenance::UserAuthored,
            retired: false,
            retired_reason: None,
            superseded_by: None,
            distilled_from: vec![],
            version: 1,
        }),
    );

    let profile = minimal_profile();
    let (workspace_ctx, mut agent_home_ctx) = empty_contexts();
    // CLI mode: precedence directive on, matching the CLI runner's call site.
    agent_home_ctx.skills_block = render_studio_skills_block(&registry, true);
    assert!(
        agent_home_ctx.skills_block.is_some(),
        "registry with one skill must render a block"
    );

    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );

    assert!(result.contains("# Studio Skills"), "skills heading missing");
    assert!(result.contains("**brainstorming**"), "enabled plugin skill missing from prompt");
    assert!(result.contains("[plugin: superpowers]"), "plugin suffix missing");
    assert!(
        result.contains("prefer the Studio skill"),
        "CLI precedence directive missing from composed prompt"
    );
}

// ── Scenario: legacy `skills` fallback when no skills_block is supplied ──

#[test]
fn legacy_skills_used_when_skills_block_absent() {
    let profile = minimal_profile();
    let (workspace_ctx, mut agent_home_ctx) = empty_contexts();
    agent_home_ctx.skills_block = None;
    agent_home_ctx.skills = vec!["Legacy skill body content.".to_string()];

    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );

    assert!(result.contains("# Studio Skills"), "skills heading missing");
    assert!(
        result.contains("Legacy skill body content."),
        "legacy skills should render when skills_block is absent"
    );
}

// ── Scenario: skills_block supersedes legacy skills (no double-render) ──

#[test]
fn skills_block_supersedes_legacy_skills() {
    let profile = minimal_profile();
    let (workspace_ctx, mut agent_home_ctx) = empty_contexts();
    agent_home_ctx.skills_block =
        Some("<system-reminder>\n# Studio Skills\nAuthoritative listing.\n</system-reminder>".to_string());
    agent_home_ctx.skills = vec!["Legacy that must NOT appear.".to_string()];

    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );

    assert!(result.contains("Authoritative listing."), "skills_block content missing");
    assert!(
        !result.contains("Legacy that must NOT appear."),
        "legacy skills must be suppressed when skills_block is present"
    );
}

// ── Scenario: un-migrated legacy `system_prompt` falls back into the persona section ──
//
// Regression: the persona migration is an explicit, user-confirmed step. Until
// it runs, a legacy profile holds only `system_prompt` (persona /
// special_instructions both None). The composer must surface that authored
// guidance rather than silently dropping it — otherwise a native/API run loses
// the agent's custom prompt entirely.

#[test]
fn legacy_system_prompt_falls_back_into_persona_section() {
    let mut profile = minimal_profile();
    profile.system_prompt = Some("You are a meticulous code reviewer.".to_string());
    profile.persona = None;
    profile.special_instructions = None;

    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );

    assert!(
        result.contains("You are a meticulous code reviewer."),
        "un-migrated legacy system_prompt must reach the composed prompt; got: {result}"
    );
}

// ── Scenario: the legacy fallback splits persona vs. imperative instructions ──

#[test]
fn legacy_system_prompt_fallback_classifies_persona_and_instructions() {
    let mut profile = minimal_profile();
    profile.system_prompt = Some(
        "You are a senior Rust engineer.\n\nAlways write tests.\nNever skip documentation."
            .to_string(),
    );
    profile.persona = None;
    profile.special_instructions = None;

    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );

    assert!(result.contains("## Persona"), "persona heading missing");
    assert!(
        result.contains("You are a senior Rust engineer."),
        "persona prose missing; got: {result}"
    );
    assert!(result.contains("## Special Instructions"), "instructions heading missing");
    assert!(
        result.contains("Always write tests."),
        "imperative line missing from instructions; got: {result}"
    );
}

// ── Scenario: migrated fields take precedence; legacy field is ignored ──
//
// Once persona/special_instructions exist, the profile is migrated and the
// legacy `system_prompt` must NOT be re-derived (which would double-render it).

#[test]
fn migrated_fields_suppress_legacy_system_prompt_fallback() {
    let mut profile = minimal_profile();
    profile.persona = Some("Migrated persona content.".to_string());
    profile.special_instructions = None;
    profile.system_prompt = Some("STALE legacy prompt that must not appear.".to_string());

    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );

    assert!(
        result.contains("Migrated persona content."),
        "migrated persona must render; got: {result}"
    );
    assert!(
        !result.contains("STALE legacy prompt"),
        "legacy system_prompt must be ignored once the profile is migrated; got: {result}"
    );
}

// ── Scenario: CLI runner mode → prefer-launchpad section present ──

#[test]
fn cli_mode_prefer_launchpad_section_present() {
    let profile = minimal_profile(); // runner_mode: AgentRunnerMode::Cli
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );
    assert!(
        result.contains("# Prefer Launchpad Tools"),
        "CLI mode must include the prefer-launchpad section; got: {result}"
    );
    assert!(
        result.contains("mcp__launchpad__Delegate"),
        "section must mention mcp__launchpad__Delegate; got: {result}"
    );
}

// ── Scenario: native/API runner mode → prefer-launchpad section absent ──

#[test]
fn native_mode_prefer_launchpad_section_absent() {
    use ao_protocol::agent::AgentRunnerMode;
    let mut profile = minimal_profile();
    profile.runner_mode = AgentRunnerMode::Api;
    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );
    assert!(
        !result.contains("# Prefer Launchpad Tools"),
        "native/API mode must NOT include the prefer-launchpad section; got: {result}"
    );
}

// ── Scenario: an all-boilerplate legacy prompt collapses to nothing ──
//
// Pre-composer prompts embedded boilerplate the composer now emits itself. The
// fallback runs the migrator (not a verbatim copy), so a prompt that is purely
// boilerplate yields no persona section and never double-renders that content.

#[test]
fn legacy_fallback_strips_boilerplate_no_double_render() {
    let mut profile = minimal_profile();
    profile.system_prompt = Some(
        "# Memory Management\n\nSave memories with tags.\n\n# Tool Selection: Direct Tools vs. Sub-Agents\n\nPrefer direct tools."
            .to_string(),
    );
    profile.persona = None;
    profile.special_instructions = None;

    let (workspace_ctx, agent_home_ctx) = empty_contexts();
    let result = compose_system_prompt(
        &profile,
        &no_name_prefs(),
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        FROZEN_DATE,
        None,
    );

    assert!(
        !result.contains("## Persona"),
        "an all-boilerplate legacy prompt must not produce a persona section; got: {result}"
    );
    // The composer still emits exactly one canonical memory-management block.
    assert_eq!(
        result.matches("Save memories").count(),
        0,
        "legacy boilerplate must not leak the literal sample text back in; got: {result}"
    );
}
