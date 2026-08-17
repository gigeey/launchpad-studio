use super::*;
use super::SkillRegister;
use ao_engine_tools_core::{
    background_agents::{
        BackgroundAgentId, ChildRunner, RunnerEvent, SubagentRegistry, SubagentSpawner,
        TaskFinalReport,
    },
    skill_registry::{SkillEntry, SkillRegistry},
    RunnerContext, ToolOutput,
};
use ao_protocol::{agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, PluginEnablement, ProviderConfig,
}, error::AoError};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

// Use the crate-wide mutex to serialise all tests that mutate the
// process-global LAUNCHPAD_STUDIO_DATA_DIR env var (shared with config and
// delegate tests).
use crate::lock_env_var;

// ─── helpers ────────────────────────────────────────────────────────────────

fn minimal_profile() -> AgentProfile {
    AgentProfile {
        id: "test-agent".to_string(),
        name: "Test".to_string(),
        description: "test".to_string(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "claude".to_string(),
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
        enabled_plugins: HashMap::new(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        runner_mode: Default::default(),
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

fn write_skill(dir: &std::path::Path, rel_path: &str, content: &str) {
    let full = dir.join(rel_path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, content).unwrap();
}

// Two SKILL.md documents used by the dispatch tests below. They are held inline
// rather than as fixture files so the test target has no out-of-crate file
// dependency: an `include_str!` of a path outside this crate turns a deleted or
// moved fixture into a compile error for the whole workspace.
//
// `verify-studio` exercises inline dispatch — its body must reach the agent
// verbatim, so the tests assert on the `studio-check-OK` marker below.
const VERIFY_STUDIO_SKILL: &str = "---
name: verify-studio
description: Outputs a deterministic verification phrase.
---
verification phrase: studio-check-OK args=$ARGUMENTS
";

// `context: fork` is the load-bearing line here — it routes the skill through
// the spawner into a child runner instead of the inline path.
const FORK_VERIFY_SKILL: &str = "---
name: fork-verify
description: A fork-dispatch skill that runs in an isolated child runner.
context: fork
---
Execute the fork verification task. Arguments: $ARGUMENTS
";

// ─── stub ChildRunner for fork dispatch tests ─────────────────────────────────

struct StubChildRunner {
    response: String,
}

impl ChildRunner for StubChildRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        let response = self.response.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            let _ = event_tx.send(RunnerEvent::AssistantText {
                background_agent_id: bg_id.clone(),
                text: response.clone(),
            });
            let _ = event_tx.send(RunnerEvent::Completed { background_agent_id: bg_id });
            Ok(TaskFinalReport::completed(Some(response)))
        })
    }
}

fn make_fork_skill_tool() -> RunSkill {
    let spawner = SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(StubChildRunner {
            response: "fork-child-output".to_string(),
        }));
    RunSkill::with_spawner(Arc::new(spawner))
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[test]
fn runskill_and_skillregister_are_cli_compatible() {
    assert!(RunSkill::new().cli_compatible(), "RunSkill must be cli_compatible");
    assert!(SkillRegister.cli_compatible(), "SkillRegister must be cli_compatible");
}

#[tokio::test]
async fn runskill_check_permissions_returns_allow() {
    // Loading a skill only injects instructions; the tools it then invokes are
    // gated individually, so the load itself is allowed unconditionally.
    use ao_engine_tools_core::{EngineTool, PermissionContext, PermissionDecision, PermissionMode};
    let pctx = PermissionContext::new(PermissionMode::Default, "agent", "session");
    let decision = RunSkill::new()
        .check_permissions(&json!({"skill": "review:foo"}), &pctx)
        .await;
    assert!(
        matches!(decision, PermissionDecision::Allow),
        "expected Allow, got: {decision:?}"
    );
}

#[tokio::test]
async fn tool_name_is_runskill() {
    assert_eq!(RunSkill::new().name(), "RunSkill");
}

#[test]
fn runskill_registered_in_registry() {
    use ao_engine_tools_core::Registry;
    let mut r = Registry::new();
    r.register_engine(Arc::new(RunSkill::new()));
    assert!(r.lookup_engine("RunSkill").is_some());
}

#[tokio::test]
async fn missing_skill_field_returns_recoverable_error() {
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"));
    let out = RunSkill::new().invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn unknown_skill_returns_recoverable_not_found_error() {
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"));
    let out = RunSkill::new().invoke(json!({"skill": "no-such-skill"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("not found"), "message: {message}");
            assert!(message.contains("no-such-skill"), "message: {message}");
        }
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn load_error_entry_returns_non_recoverable_error() {
    let tmp = tempfile::tempdir().unwrap();
    // SKILL.md with no name/description → parse error
    write_skill(tmp.path(), "skills/bad/SKILL.md", "---\nno_name: true\n---\nbody\n");

    let mut profile = minimal_profile();
    profile.skills = vec!["bad".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);
    let out = RunSkill::new().invoke(json!({"skill": "bad"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(!recoverable, "load errors should not be recoverable");
            assert!(message.contains("bad"), "message: {message}");
            assert!(message.contains("failed to load"), "message: {message}");
        }
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn fork_skill_dispatches_via_spawner() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/fork-verify/SKILL.md", FORK_VERIFY_SKILL);

    let mut profile = minimal_profile();
    profile.skills = vec!["fork-verify".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let skill_tool = make_fork_skill_tool();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let out = skill_tool
        .invoke(json!({"skill": "fork-verify", "args": "hello"}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => {
            assert!(
                s.contains("fork-child-output"),
                "fork output should contain child text, got: {s}"
            );
            assert!(
                s.contains("Skill \"fork-verify\" completed"),
                "fork output should have completion header, got: {s}"
            );
        }
        _ => panic!("expected Text from fork dispatch, got: {:?}", out),
    }

    // Fork dispatch must NOT enqueue into pending_user_messages.
    let msgs = ctx.pending_user_messages.lock().unwrap();
    assert!(msgs.is_empty(), "fork dispatch must not enqueue user messages");
}

#[tokio::test]
async fn fork_skill_no_spawner_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/fork-verify/SKILL.md", FORK_VERIFY_SKILL);

    let mut profile = minimal_profile();
    profile.skills = vec!["fork-verify".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let skill_tool = RunSkill::new();
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let out = skill_tool.invoke(json!({"skill": "fork-verify"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(!recoverable),
        _ => panic!("expected non-recoverable error when no spawner configured"),
    }
}

#[tokio::test]
async fn verify_studio_inline_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/verify-studio/SKILL.md", VERIFY_STUDIO_SKILL);

    let mut profile = minimal_profile();
    profile.skills = vec!["verify-studio".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let out = RunSkill::new()
        .invoke(json!({"skill": "verify-studio", "args": "test-arg"}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => assert_eq!(s, "Launching skill: verify-studio"),
        _ => panic!("expected Text, got: {:?}", out),
    }

    let msgs = ctx.pending_user_messages.lock().unwrap();
    assert_eq!(msgs.len(), 1, "exactly one message should be enqueued");
    let msg = &msgs[0];
    assert!(
        msg.contains("studio-check-OK"),
        "message should contain studio-check-OK, got: {msg}"
    );
    assert!(
        msg.contains("test-arg"),
        "$ARGUMENTS should be replaced with test-arg, got: {msg}"
    );
    assert!(
        !msg.contains("$ARGUMENTS"),
        "$ARGUMENTS placeholder should be gone, got: {msg}"
    );
}

#[tokio::test]
async fn inline_skill_returns_body_as_result_in_single_call_dispatch() {
    // Single-call dispatch (the MCP HTTP route) has no turn loop draining
    // pending_user_messages, so the inline body must come back via the tool
    // result. Regression guard: previously the body was enqueued onto a
    // per-request context that was immediately dropped, and the skill body
    // never reached the externally-driven agent.
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/verify-studio/SKILL.md", VERIFY_STUDIO_SKILL);

    let mut profile = minimal_profile();
    profile.skills = vec!["verify-studio".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_inline_skill_via_tool_result()
        .with_skill_registry(registry);

    let out = RunSkill::new()
        .invoke(json!({"skill": "verify-studio", "args": "test-arg"}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => {
            assert!(
                s.contains("studio-check-OK"),
                "tool result should carry the skill body, got: {s}"
            );
            assert!(
                s.contains("test-arg"),
                "$ARGUMENTS should be substituted in the returned body, got: {s}"
            );
            assert_ne!(
                s, "Launching skill: verify-studio",
                "single-call dispatch must NOT return the launch acknowledgement"
            );
        }
        _ => panic!("expected Text, got: {:?}", out),
    }

    // Nothing may be enqueued — there is no loop to drain it here.
    let msgs = ctx.pending_user_messages.lock().unwrap();
    assert!(
        msgs.is_empty(),
        "single-call dispatch must not enqueue user messages; got {} message(s)",
        msgs.len()
    );
}

#[tokio::test]
async fn arguments_default_to_empty_string_when_omitted() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "skills/simple/SKILL.md",
        "---\nname: simple\ndescription: Simple\n---\nargs: $ARGUMENTS\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["simple".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let out = RunSkill::new().invoke(json!({"skill": "simple"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(s) => assert_eq!(s, "Launching skill: simple"),
        _ => panic!("expected Text"),
    }

    let msgs = ctx.pending_user_messages.lock().unwrap();
    // Inline-skill bodies are wrapped in the
    // `[skill "<name>" loaded]\n<body>` chip-coalesce envelope before
    // being enqueued.
    assert_eq!(msgs[0], "[skill \"simple\" loaded]\nargs: \n");
}

#[tokio::test]
async fn session_and_agent_id_substituted() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "skills/ids/SKILL.md",
        "---\nname: ids\ndescription: IDs\n---\nsession=${SESSION_ID} agent=${AGENT_ID}\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["ids".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("my-session", "my-agent", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    RunSkill::new().invoke(json!({"skill": "ids"}), &ctx).await.unwrap();

    let msgs = ctx.pending_user_messages.lock().unwrap();
    // Inline-skill bodies are wrapped in the chip-coalesce
    // envelope (`[skill "<name>" loaded]\n<body>`).
    assert_eq!(
        msgs[0],
        "[skill \"ids\" loaded]\nsession=my-session agent=my-agent\n"
    );
}

#[tokio::test]
async fn all_five_substitution_vars_resolve() {
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    write_skill(
        tmp.path(),
        "skills/five-vars/SKILL.md",
        "---\nname: five-vars\ndescription: All five vars\n---\ndata=${LAUNCHPAD_DATA_DIR} skill=${LAUNCHPAD_SKILL_DIR} sess=${SESSION_ID} agent=${AGENT_ID} args=$ARGUMENTS\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["five-vars".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("my-sess", "my-agent", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let out = RunSkill::new()
        .invoke(json!({"skill": "five-vars", "args": "hello-arg"}), &ctx)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)), "expected Text, got: {out:?}");

    let msgs = ctx.pending_user_messages.lock().unwrap();
    let body = &msgs[0];

    let data_dir_str = tmp.path().to_string_lossy().into_owned();
    let skill_dir_str = format!("{}/skills/five-vars/", data_dir_str);

    assert!(body.contains(&data_dir_str), "LAUNCHPAD_DATA_DIR not substituted; body: {body}");
    assert!(body.contains(&skill_dir_str), "LAUNCHPAD_SKILL_DIR not substituted; body: {body}");
    assert!(body.contains("my-sess"), "SESSION_ID not substituted; body: {body}");
    assert!(body.contains("my-agent"), "AGENT_ID not substituted; body: {body}");
    assert!(body.contains("hello-arg"), "$ARGUMENTS not substituted; body: {body}");
    assert!(!body.contains("${LAUNCHPAD_DATA_DIR}"), "placeholder still present; body: {body}");
    assert!(!body.contains("${LAUNCHPAD_SKILL_DIR}"), "placeholder still present; body: {body}");
    assert!(!body.contains("$ARGUMENTS"), "placeholder still present; body: {body}");
}

#[tokio::test]
async fn allowed_tools_filter_is_set_after_inline_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "skills/filtered/SKILL.md",
        "---\nname: filtered\ndescription: Filter test\nallowed-tools:\n  - Read\n  - Grep\n---\nbody\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["filtered".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let out = RunSkill::new().invoke(json!({"skill": "filtered"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(s) => assert_eq!(s, "Launching skill: filtered"),
        _ => panic!("expected Text"),
    }

    assert!(ctx.skill_tool_filter.read().unwrap().is_some(), "filter should be set");
    assert!(ctx.check_skill_tool_filter("Read"), "Read should be allowed");
    assert!(ctx.check_skill_tool_filter("Grep"), "Grep should be allowed");
    assert!(!ctx.check_skill_tool_filter("Bash"), "Bash should be denied");
}

#[tokio::test]
async fn skill_invoke_bumps_usage_counter() {
    use ao_engine_tools_core::skill_registry::usage;

    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    write_skill(
        tmp.path(),
        "skills/counter-skill/SKILL.md",
        "---\nname: counter-skill\ndescription: Counter test skill\n---\nDo something.\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["counter-skill".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("sess", "test-agent", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let out = RunSkill::new()
        .invoke(json!({"skill": "counter-skill"}), &ctx)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)), "expected Text, got: {out:?}");

    // Usage is stored per-agent at agent_homes/<agent_id>/skills/.usage.json
    let usage_dir = tmp.path().join("agent_homes").join("test-agent").join("skills");
    let map = usage::load(&usage_dir).await;
    assert_eq!(
        map.get("counter-skill").map(|e| e.count),
        Some(1),
        "count should be 1 after first invoke"
    );

    RunSkill::new()
        .invoke(json!({"skill": "counter-skill"}), &ctx)
        .await
        .unwrap();

    let map = usage::load(&usage_dir).await;
    assert_eq!(
        map.get("counter-skill").map(|e| e.count),
        Some(2),
        "count should be 2 after second invoke"
    );
}

#[tokio::test]
async fn runskill_and_skillregister_always_pass_filter() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "skills/filtered2/SKILL.md",
        "---\nname: filtered2\ndescription: Filter test 2\nallowed-tools:\n  - Read\n---\nbody\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["filtered2".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    RunSkill::new().invoke(json!({"skill": "filtered2"}), &ctx).await.unwrap();

    assert!(ctx.check_skill_tool_filter("RunSkill"), "RunSkill must always pass");
    assert!(ctx.check_skill_tool_filter("SkillRegister"), "SkillRegister must always pass");
}

#[tokio::test]
async fn no_allowed_tools_leaves_filter_unset() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "skills/open/SKILL.md",
        "---\nname: open\ndescription: No filter\n---\nbody\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["open".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    RunSkill::new().invoke(json!({"skill": "open"}), &ctx).await.unwrap();

    assert!(
        ctx.skill_tool_filter.read().unwrap().is_none(),
        "filter should not be set when skill has no allowed-tools"
    );
}

#[tokio::test]
async fn skill_entry_ok_check() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/verify-studio/SKILL.md", VERIFY_STUDIO_SKILL);

    let mut profile = minimal_profile();
    profile.skills = vec!["verify-studio".to_string()];
    let registry = SkillRegistry::load(tmp.path(), &profile);

    match registry.get("verify-studio").expect("should be present") {
        SkillEntry::Ok(r) => {
            assert_eq!(r.name, "verify-studio");
            assert_eq!(r.description, "Outputs a deterministic verification phrase.");
        }
        SkillEntry::Err(e) => panic!("unexpected load error: {e}"),
    }
}

// ─── SkillRegister helpers ────────────────────────────────────────────────────

fn write_profile(data_dir: &std::path::Path, profile: &AgentProfile) {
    let agents_dir = data_dir.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let yaml = serde_yaml::to_string(profile).unwrap();
    std::fs::write(agents_dir.join(format!("{}.yaml", profile.id)), yaml).unwrap();
}

fn make_ctx_for_skillwrite(data_dir: &std::path::Path) -> RunnerContext {
    let profile = minimal_profile();
    write_profile(data_dir, &profile);
    RunnerContext::new_with_cwd("sess", &profile.id, PathBuf::from("/tmp"))
}

const VALID_SKILL_BODY: &str =
    "---\nname: my-skill\ndescription: A test skill\n---\nDo something useful.\n";

// ─── SkillRegister tests ─────────────────────────────────────────────────────

#[test]
fn skillregister_tool_name() {
    assert_eq!(SkillRegister.name(), "SkillRegister");
}

#[tokio::test]
async fn skillwrite_name_format_rejected() {
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"));

    let invalid_names: &[&str] = &[
        "Bad Name",  // uppercase + space
        "a/b",       // contains slash
        ".hidden",   // starts with dot
        &"a".repeat(65), // 65 chars > 64
        "UPPER",     // uppercase
        "has space", // space
    ];

    for bad_name in invalid_names {
        let out = SkillRegister
            .invoke(
                json!({"name": bad_name, "description": "ok", "body": VALID_SKILL_BODY}),
                &ctx,
            )
            .await
            .unwrap();
        match out {
            ToolOutput::Error { message, recoverable } => {
                assert!(recoverable, "name validation error should be recoverable, name={bad_name}");
                assert!(message.contains("invalid skill name"), "bad message for '{bad_name}': {message}");
            }
            _ => panic!("expected Error for name '{bad_name}'"),
        }
    }
}

#[tokio::test]
async fn skillwrite_description_validation() {
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"));

    // Empty description
    let out = SkillRegister
        .invoke(json!({"name": "valid-name", "description": "", "body": VALID_SKILL_BODY}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("description must be 1-240"), "msg: {message}");
        }
        _ => panic!("expected Error for empty description"),
    }

    // >240 char description
    let long_desc = "x".repeat(241);
    let out = SkillRegister
        .invoke(json!({"name": "valid-name", "description": long_desc, "body": VALID_SKILL_BODY}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("description must be 1-240"), "msg: {message}");
        }
        _ => panic!("expected Error for too-long description"),
    }
}

#[tokio::test]
async fn skillwrite_happy_path() {
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = make_ctx_for_skillwrite(tmp.path());

    let body = "---\nname: my-skill\ndescription: A test skill\n---\nDo something useful.\n";
    let out = SkillRegister
        .invoke(
            json!({"name": "my-skill", "description": "A test skill", "body": body}),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        // Trust gate: a self-authored skill always stages for
        // review rather than going live — see `disable_model_invocation_*`
        // tests below for the full quarantine/confirm lifecycle.
        ToolOutput::Text(s) => {
            assert!(s.contains("my-skill"), "output: {s}");
            assert!(s.contains("staged for review"), "output: {s}");
        }
        _ => panic!("expected Text, got: {out:?}"),
    }

    // SKILL.md written to disk, gated: `disable-model-invocation: true` is
    // forced into the frontmatter even though the submitted body had no
    // such key.
    let skill_path = tmp.path().join("skills").join("my-skill").join("SKILL.md");
    assert!(skill_path.exists(), "SKILL.md should exist at {:?}", skill_path);
    let written = std::fs::read_to_string(&skill_path).unwrap();
    assert_ne!(written, body, "gate must rewrite the body to add the quarantine flag");
    assert!(
        written.contains("disable-model-invocation: true"),
        "written SKILL.md should be quarantined, got: {written}"
    );

    // Profile updated on disk
    let profile_path = tmp.path().join("agents").join("test-agent.yaml");
    let profile: AgentProfile =
        serde_yaml::from_str(&std::fs::read_to_string(&profile_path).unwrap()).unwrap();
    assert!(profile.skills.contains(&"my-skill".to_string()), "skills: {:?}", profile.skills);

    // Registry hot-replaced
    let registry = ctx.skill_registry.read().unwrap();
    match registry.get("my-skill") {
        Some(SkillEntry::Ok(record)) => {
            assert!(record.disable_model_invocation, "newly-registered skill must be quarantined")
        }
        other => panic!("expected SkillEntry::Ok(quarantined), got: {other:?}"),
    }
}

#[tokio::test]
async fn skillwrite_skill_exists_without_override() {
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = make_ctx_for_skillwrite(tmp.path());
    let body = "---\nname: my-skill\ndescription: A test skill\n---\nDo something useful.\n";

    // First write succeeds
    SkillRegister
        .invoke(json!({"name": "my-skill", "description": "ok", "body": body}), &ctx)
        .await
        .unwrap();

    // Second write without override should fail
    let out = SkillRegister
        .invoke(json!({"name": "my-skill", "description": "ok", "body": body}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable, "SkillExists should be recoverable");
            assert!(message.contains("SkillExists"), "msg: {message}");
        }
        _ => panic!("expected SkillExists error, got: {out:?}"),
    }
}

#[tokio::test]
async fn skillwrite_override_true_updates_user_pool() {
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = make_ctx_for_skillwrite(tmp.path());
    let body_v1 = "---\nname: my-skill\ndescription: A test skill\n---\nVersion 1.\n";
    let body_v2 = "---\nname: my-skill\ndescription: A test skill\n---\nVersion 2.\n";

    // First write
    SkillRegister
        .invoke(json!({"name": "my-skill", "description": "ok", "body": body_v1}), &ctx)
        .await
        .unwrap();

    // Second write with override:true should succeed
    let out = SkillRegister
        .invoke(
            json!({"name": "my-skill", "description": "ok", "body": body_v2, "override": true}),
            &ctx,
        )
        .await
        .unwrap();
    match &out {
        ToolOutput::Text(s) => assert!(s.contains("my-skill"), "output: {s}"),
        _ => panic!("expected Text on override, got: {out:?}"),
    }

    // Verify SKILL.md has been updated with the new body content, still
    // gated: re-registering (even as an update) re-quarantines rather than
    // carrying forward any prior confirmation — the new body hasn't been
    // reviewed either.
    let skill_path = tmp.path().join("skills").join("my-skill").join("SKILL.md");
    let written = std::fs::read_to_string(&skill_path).unwrap();
    assert!(written.contains("Version 2."), "written: {written}");
    assert!(
        written.contains("disable-model-invocation: true"),
        "override must re-quarantine, got: {written}"
    );
}

// ─── provenance + versioning ────────────────────────────────────────────────

#[tokio::test]
async fn skillwrite_override_bumps_version() {
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = make_ctx_for_skillwrite(tmp.path());
    let body_v1 = "---\nname: my-skill\ndescription: A test skill\n---\nAttempt one.\n";
    let body_v2 = "---\nname: my-skill\ndescription: A test skill\n---\nAttempt two.\n";
    let body_v3 = "---\nname: my-skill\ndescription: A test skill\n---\nAttempt three.\n";

    // First registration starts at version 1.
    SkillRegister
        .invoke(json!({"name": "my-skill", "description": "ok", "body": body_v1}), &ctx)
        .await
        .unwrap();
    {
        let registry = ctx.skill_registry.read().unwrap();
        match registry.get("my-skill") {
            Some(SkillEntry::Ok(record)) => {
                assert_eq!(record.version, 1, "a brand-new skill starts at version 1")
            }
            other => panic!("expected SkillEntry::Ok, got: {other:?}"),
        }
    }

    // Re-registering over the same name bumps the version by 1 each time.
    SkillRegister
        .invoke(
            json!({"name": "my-skill", "description": "ok", "body": body_v2, "override": true}),
            &ctx,
        )
        .await
        .unwrap();
    {
        let registry = ctx.skill_registry.read().unwrap();
        match registry.get("my-skill") {
            Some(SkillEntry::Ok(record)) => {
                assert_eq!(record.version, 2, "the first override must bump to version 2")
            }
            other => panic!("expected SkillEntry::Ok, got: {other:?}"),
        }
    }

    SkillRegister
        .invoke(
            json!({"name": "my-skill", "description": "ok", "body": body_v3, "override": true}),
            &ctx,
        )
        .await
        .unwrap();
    {
        let registry = ctx.skill_registry.read().unwrap();
        match registry.get("my-skill") {
            Some(SkillEntry::Ok(record)) => {
                assert_eq!(record.version, 3, "the second override must bump to version 3")
            }
            other => panic!("expected SkillEntry::Ok, got: {other:?}"),
        }
    }
}

#[tokio::test]
async fn skillwrite_collides_with_plugin_pool() {
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    // Set up a plugin-pool skill "plugin-skill" in the registry
    let plugin_skill_dir = tmp
        .path()
        .join("plugins")
        .join("superpowers")
        .join("skills")
        .join("plugin-skill");
    std::fs::create_dir_all(&plugin_skill_dir).unwrap();
    std::fs::write(
        plugin_skill_dir.join("SKILL.md"),
        "---\nname: plugin-skill\ndescription: From plugin\n---\nbody\n",
    )
    .unwrap();

    // Build a profile that enables the plugin
    let mut profile = minimal_profile();
    profile.enabled_plugins.insert(
        "superpowers".to_string(),
        PluginEnablement { enabled: true, enabled_skills: None },
    );
    write_profile(tmp.path(), &profile);

    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));
    let ctx = RunnerContext::new_with_cwd("sess", &profile.id, PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let body = "---\nname: plugin-skill\ndescription: Trying to override\n---\nbody\n";
    let out = SkillRegister
        .invoke(
            json!({"name": "plugin-skill", "description": "test", "body": body, "override": true}),
            &ctx,
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable, "collision should be recoverable");
            assert!(
                message.contains("SkillCollidesWithPlugin"),
                "msg: {message}"
            );
        }
        _ => panic!("expected SkillCollidesWithPlugin error, got: {out:?}"),
    }
}

// ─── tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn leading_slash_normalized_before_lookup() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "skills/review-pr/SKILL.md",
        "---\nname: review-pr\ndescription: Review a PR\n---\nDo the review.\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["review-pr".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    // Call with leading slash — must resolve exactly as "review-pr".
    let out = RunSkill::new()
        .invoke(json!({"skill": "/review-pr"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(
            s.contains("review-pr"),
            "expected Launching skill: review-pr, got: {s}"
        ),
        ToolOutput::Error { message, .. } => {
            panic!("expected success with slash-prefixed name, got error: {message}")
        }
        _ => panic!("expected Text"),
    }
}

#[tokio::test]
async fn leading_slash_not_double_stripped() {
    // A name of "//foo" should only strip one slash → "foo" is not found.
    // (The outer slash becomes "/foo" which still doesn't exist.)
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"));

    let out = RunSkill::new()
        .invoke(json!({"skill": "//foo"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("not found"), "msg: {message}");
        }
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn plugin_qualified_name_resolves() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_skill_dir = tmp
        .path()
        .join("plugins")
        .join("superpowers")
        .join("skills")
        .join("pdf");
    std::fs::create_dir_all(&plugin_skill_dir).unwrap();
    std::fs::write(
        plugin_skill_dir.join("SKILL.md"),
        "---\nname: pdf\ndescription: Convert to PDF\n---\nConvert this.\n",
    )
    .unwrap();

    let mut profile = minimal_profile();
    profile.enabled_plugins.insert(
        "superpowers".to_string(),
        ao_protocol::agent::PluginEnablement { enabled: true, enabled_skills: None },
    );
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    // Qualified form must resolve to the plugin skill.
    let out = RunSkill::new()
        .invoke(json!({"skill": "superpowers:pdf"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("pdf"), "expected Launching skill: pdf, got: {s}"),
        ToolOutput::Error { message, .. } => {
            panic!("plugin:skill lookup failed: {message}")
        }
        _ => panic!("expected Text"),
    }
}

#[tokio::test]
async fn plugin_qualified_wrong_plugin_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_skill_dir = tmp
        .path()
        .join("plugins")
        .join("superpowers")
        .join("skills")
        .join("pdf");
    std::fs::create_dir_all(&plugin_skill_dir).unwrap();
    std::fs::write(
        plugin_skill_dir.join("SKILL.md"),
        "---\nname: pdf\ndescription: Convert to PDF\n---\nConvert this.\n",
    )
    .unwrap();

    let mut profile = minimal_profile();
    profile.enabled_plugins.insert(
        "superpowers".to_string(),
        ao_protocol::agent::PluginEnablement { enabled: true, enabled_skills: None },
    );
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    // Wrong plugin prefix → not found.
    let out = RunSkill::new()
        .invoke(json!({"skill": "wrongplugin:pdf"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        _ => panic!("expected Error for wrong plugin prefix"),
    }
}

// ─── tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn disable_model_invocation_returns_recoverable_error() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "skills/gated/SKILL.md",
        "---\nname: gated\ndescription: Gated skill\ndisable-model-invocation: true\n---\nbody\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["gated".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let out = RunSkill::new()
        .invoke(json!({"skill": "gated"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable, "disable-model-invocation error should be recoverable");
            assert!(
                message.contains("not available for model invocation"),
                "msg: {message}"
            );
        }
        _ => panic!("expected Error for disable-model-invocation skill"),
    }
}

#[tokio::test]
async fn fork_result_wrapped_with_contextual_header() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/fork-verify/SKILL.md", FORK_VERIFY_SKILL);

    let mut profile = minimal_profile();
    profile.skills = vec!["fork-verify".to_string()];
    let registry = Arc::new(SkillRegistry::load(tmp.path(), &profile));

    let skill_tool = make_fork_skill_tool();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_skill_registry(registry);

    let out = skill_tool
        .invoke(json!({"skill": "fork-verify"}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => {
            assert!(s.starts_with("Skill \"fork-verify\" completed."), "missing header, got: {s}");
            assert!(s.contains("Result:"), "missing Result label, got: {s}");
            assert!(s.contains("fork-child-output"), "missing child output, got: {s}");
        }
        _ => panic!("expected Text, got: {:?}", out),
    }
}

#[tokio::test]
async fn model_frontmatter_parsed_and_stored() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "skills/fast-skill/SKILL.md",
        "---\nname: fast-skill\ndescription: Uses fast model\nmodel: claude-haiku-4-5\ncontext: fork\n---\nbody\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["fast-skill".to_string()];
    let registry = SkillRegistry::load(tmp.path(), &profile);

    match registry.get("fast-skill").unwrap() {
        ao_engine_tools_core::skill_registry::SkillEntry::Ok(r) => {
            assert_eq!(r.model.as_deref(), Some("claude-haiku-4-5"));
        }
        ao_engine_tools_core::skill_registry::SkillEntry::Err(e) => {
            panic!("unexpected load error: {e}")
        }
    }
}

#[tokio::test]
async fn when_to_use_frontmatter_parsed_and_stored() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "skills/discover/SKILL.md",
        "---\nname: discover\ndescription: Discover things\nwhen-to-use: Use for code discovery tasks\n---\nbody\n",
    );

    let mut profile = minimal_profile();
    profile.skills = vec!["discover".to_string()];
    let registry = SkillRegistry::load(tmp.path(), &profile);

    match registry.get("discover").unwrap() {
        ao_engine_tools_core::skill_registry::SkillEntry::Ok(r) => {
            assert_eq!(
                r.when_to_use.as_deref(),
                Some("Use for code discovery tasks")
            );
        }
        ao_engine_tools_core::skill_registry::SkillEntry::Err(e) => {
            panic!("unexpected load error: {e}")
        }
    }
}

// ─── trust gate tests ───────────────────────────────────────────────────
//
// Re-establishes the review boundary: a
// self-authored skill must not be model-invocable until confirmed.

#[tokio::test]
async fn skillregister_skill_is_reviewable_and_runnable_after_approval() {
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = make_ctx_for_skillwrite(tmp.path());
    let body = "---\nname: quarantine-me\ndescription: Needs review\n---\nDo the risky thing.\n";

    SkillRegister
        .invoke(
            json!({"name": "quarantine-me", "description": "Needs review", "body": body}),
            &ctx,
        )
        .await
        .unwrap();

    // Not model-invocable yet — the default quarantine holds.
    let out = RunSkill::new().invoke(json!({"skill": "quarantine-me"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(
                message.contains("not available for model invocation"),
                "msg: {message}"
            );
        }
        _ => panic!("expected decline before confirmation, got: {out:?}"),
    }

    // Confirm it the way a human actually can — through the review queue the
    // Studio UI drives, not by poking the frontmatter primitive directly.
    // Asserting that `list_queue` LISTS the skill is the load-bearing half:
    // `accept` alone would pass against a queue that never surfaces the skill
    // to a human, which is a state the product cannot recover from.
    let profile_path = tmp.path().join("agents").join("test-agent.yaml");
    let profile: AgentProfile =
        serde_yaml::from_str(&std::fs::read_to_string(&profile_path).unwrap()).unwrap();
    let registry = SkillRegistry::load(tmp.path(), &profile);

    let staging = ao_persistence::reflection_staging::ReflectionStagingStore::new(
        ao_persistence::paths::DataRoot::new(tmp.path()),
    );
    let queue = crate::skill::review::list_queue(tmp.path(), &registry, &staging, "test-agent")
        .await
        .unwrap();
    let listed = queue
        .candidates
        .iter()
        .find(|c| c.name == "quarantine-me")
        .expect("a SkillRegister-written skill must appear in the review queue");
    assert_eq!(
        listed.origin, "user_authored",
        "SkillRegister does not stamp `origin: distilled`"
    );

    crate::skill::review::accept(tmp.path(), &registry, "quarantine-me").await.unwrap();

    let profile: AgentProfile =
        serde_yaml::from_str(&std::fs::read_to_string(&profile_path).unwrap()).unwrap();
    ctx.replace_skill_registry(Arc::new(SkillRegistry::load(tmp.path(), &profile)));

    let out = RunSkill::new().invoke(json!({"skill": "quarantine-me"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(s) => assert_eq!(s, "Launching skill: quarantine-me"),
        _ => panic!("expected success after confirmation, got: {out:?}"),
    }
}

#[tokio::test]
async fn skillregister_never_auto_enables_even_a_narrow_readonly_skill() {
    // Every model-invocable skill stages
    // for review, full stop -- there is no low-stakes/narrow-tool-allowlist
    // carve-out (the old `classify_skill_stakes` classifier this test used
    // to exercise has been removed). A self-authored skill restricted to
    // read-only tools must stage exactly like an unrestricted one.
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = make_ctx_for_skillwrite(tmp.path());
    let body = "---\nname: safe-lookup\ndescription: Read-only lookup\nallowed-tools:\n  \
                - Read\n  - Grep\n---\nLook something up.\n";

    let out = SkillRegister
        .invoke(
            json!({"name": "safe-lookup", "description": "Read-only lookup", "body": body}),
            &ctx,
        )
        .await
        .unwrap();
    match &out {
        ToolOutput::Text(s) => {
            assert!(s.contains("staged for review"), "output: {s}");
            assert!(!s.contains("written and enabled"), "output: {s}");
        }
        _ => panic!("expected Text, got: {out:?}"),
    }

    // Not model-invocable — no auto-enable path exists for a skill,
    // regardless of how narrow its allowed-tools list is.
    let out = RunSkill::new().invoke(json!({"skill": "safe-lookup"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { message, .. } => {
            assert!(
                message.contains("not available for model invocation"),
                "msg: {message}"
            );
        }
        _ => panic!("expected decline before confirmation, got: {out:?}"),
    }
}

#[tokio::test]
async fn skillregister_forces_gate_value_over_bodys_own_claim() {
    // A self-authored skill that tries to set `disable-model-invocation:
    // false` on itself must not succeed in bypassing the gate — the model
    // that wrote the body is not a trustworthy judge of its own
    // invocability.
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = make_ctx_for_skillwrite(tmp.path());
    let body = "---\nname: self-confirmed\ndescription: Tries to self-approve\n\
                disable-model-invocation: false\n---\nDo something.\n";

    SkillRegister
        .invoke(
            json!({
                "name": "self-confirmed",
                "description": "Tries to self-approve",
                "body": body
            }),
            &ctx,
        )
        .await
        .unwrap();

    let out = RunSkill::new().invoke(json!({"skill": "self-confirmed"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { message, .. } => {
            assert!(
                message.contains("not available for model invocation"),
                "gate value must win over body's own claim; msg: {message}"
            );
        }
        _ => panic!("expected decline, got: {out:?}"),
    }
}

#[tokio::test]
async fn skillregister_fork_skill_not_auto_enabled_even_with_readonly_tools() {
    // Every self-authored skill stages for review regardless of dispatch
    // mode or allowed-tools list -- fork dispatch is
    // just one more case that must never auto-enable.
    let _lock = lock_env_var();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = make_ctx_for_skillwrite(tmp.path());
    let body = "---\nname: fork-lookup\ndescription: Forked lookup\ncontext: fork\n\
                allowed-tools:\n  - Read\n---\nLook something up.\n";

    let out = SkillRegister
        .invoke(
            json!({"name": "fork-lookup", "description": "Forked lookup", "body": body}),
            &ctx,
        )
        .await
        .unwrap();
    match &out {
        ToolOutput::Text(s) => assert!(s.contains("staged for review"), "output: {s}"),
        _ => panic!("expected Text, got: {out:?}"),
    }
}
