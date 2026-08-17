use std::collections::HashMap;
use std::path::Path;

use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, PluginEnablement, ProviderConfig,
};
use ao_search_index::{ArtifactKind, IndexScope, SearchFilter, SearchIndex};

use super::frontmatter::{
    parse_frontmatter, set_body, set_description, set_disable_model_invocation, FrontmatterError,
};
use super::search_index::{reindex_skills, skill_index_records};
use super::sources::load_builtin_pool;
use super::{ContextMode, SkillArgument, SkillEntry, SkillRegistry, SkillSource};

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

fn write_skill(dir: &Path, rel_path: &str, content: &str) {
    let full = dir.join(rel_path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, content).unwrap();
}

const SKILL_A: &str = "---\nname: skill-a\ndescription: Skill A\n---\nbody-a\n";
const SKILL_B: &str = "---\nname: skill-b\ndescription: Skill B\n---\nbody-b\n";
const SKILL_BAD: &str = "---\nno-name-or-title: true\n---\nbody\n";

#[test]
fn parse_basic_skill() {
    let content = "---\nname: my-skill\ndescription: Does something useful\n---\nBody content here.\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.name, "my-skill");
    assert_eq!(record.description, "Does something useful");
    assert_eq!(record.body, "Body content here.\n");
    assert_eq!(record.context, ContextMode::Inline);
    assert!(record.allowed_tools.is_empty());
    assert!(record.arguments.is_empty());
    assert_eq!(record.source, SkillSource::User);
}

#[test]
fn title_is_alias_for_name() {
    let content = "---\ntitle: my-skill\ndescription: A skill\n---\nbody\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.name, "my-skill");
}

#[test]
fn name_takes_precedence_over_title() {
    let content = "---\nname: primary-name\ntitle: secondary-title\ndescription: A skill\n---\nbody\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.name, "primary-name");
}

#[test]
fn unknown_frontmatter_keys_tolerated() {
    let content = "---\nname: my-skill\ndescription: A skill\nfuture_key: some_value\nanother: 42\n---\nbody\n";
    assert!(parse_frontmatter(content).is_ok(), "unknown keys must not cause an error");
}

#[test]
fn missing_name_and_title_returns_missing_required() {
    let content = "---\ndescription: A skill\n---\nbody\n";
    let err = parse_frontmatter(content).unwrap_err();
    assert!(matches!(err, FrontmatterError::MissingRequired { .. }));
}

#[test]
fn missing_description_returns_missing_required() {
    let content = "---\nname: my-skill\n---\nbody\n";
    let err = parse_frontmatter(content).unwrap_err();
    assert!(matches!(err, FrontmatterError::MissingRequired { .. }));
}

#[test]
fn malformed_yaml_returns_parse_error() {
    let content = "---\n{invalid: [yaml\n---\nbody\n";
    let err = parse_frontmatter(content).unwrap_err();
    assert!(matches!(err, FrontmatterError::ParseError { .. }));
}

#[test]
fn context_defaults_to_inline() {
    let content = "---\nname: my-skill\ndescription: A skill\n---\nbody\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.context, ContextMode::Inline);
}

#[test]
fn context_fork_parsed() {
    let content = "---\nname: my-skill\ndescription: A skill\ncontext: fork\n---\nbody\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.context, ContextMode::Fork);
}

#[test]
fn context_inline_explicit() {
    let content = "---\nname: my-skill\ndescription: A skill\ncontext: inline\n---\nbody\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.context, ContextMode::Inline);
}

#[test]
fn allowed_tools_parsed() {
    let content =
        "---\nname: my-skill\ndescription: A skill\nallowed-tools:\n  - Read\n  - Grep\n---\nbody\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.allowed_tools, vec!["Read", "Grep"]);
}

#[test]
fn arguments_parsed() {
    let content = "---\nname: my-skill\ndescription: A skill\narguments:\n  - name: input\n    required: true\n  - name: optional-arg\n---\nbody\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.arguments.len(), 2);
    assert_eq!(record.arguments[0], SkillArgument { name: "input".to_string(), required: true });
    assert_eq!(
        record.arguments[1],
        SkillArgument { name: "optional-arg".to_string(), required: false }
    );
}

#[test]
fn body_is_content_after_closing_delimiter() {
    let content =
        "---\nname: my-skill\ndescription: A skill\n---\nThis is the body.\nWith multiple lines.\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.body, "This is the body.\nWith multiple lines.\n");
}

#[test]
fn empty_body_is_ok() {
    let content = "---\nname: my-skill\ndescription: A skill\n---\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.body, "");
}

#[test]
fn no_opening_delimiter_returns_parse_error() {
    let content = "name: my-skill\ndescription: A skill\n---\nbody\n";
    let err = parse_frontmatter(content).unwrap_err();
    assert!(matches!(err, FrontmatterError::ParseError { .. }));
}

#[test]
fn no_closing_delimiter_returns_parse_error() {
    let content = "---\nname: my-skill\ndescription: A skill\n";
    let err = parse_frontmatter(content).unwrap_err();
    assert!(matches!(err, FrontmatterError::ParseError { .. }));
}

#[test]
fn skill_registry_empty_has_no_entries() {
    let registry = SkillRegistry::empty();
    assert!(registry.entries.is_empty());
}

// ─── SkillRegistry loader ───────────────────────────────────────────────────

#[test]
fn user_pool_loading() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/skill-a/SKILL.md", SKILL_A);

    let mut profile = minimal_profile();
    profile.skills = vec!["skill-a".to_string()];

    let registry = SkillRegistry::load(tmp.path(), &profile);
    let entry = registry.get("skill-a").expect("skill-a should be present");
    let SkillEntry::Ok(record) = entry else { panic!("expected Ok entry") };
    assert_eq!(record.name, "skill-a");
    assert_eq!(record.source, SkillSource::User);
}

#[test]
fn plugin_pool_loading_with_enabled_skills_subset() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "plugins/my-plugin/skills/skill-a/SKILL.md", SKILL_A);
    write_skill(tmp.path(), "plugins/my-plugin/skills/skill-b/SKILL.md", SKILL_B);

    let mut profile = minimal_profile();
    profile.enabled_plugins.insert(
        "my-plugin".to_string(),
        PluginEnablement {
            enabled: true,
            enabled_skills: Some(vec!["skill-a".to_string()]), // only skill-a
        },
    );

    let registry = SkillRegistry::load(tmp.path(), &profile);
    assert!(registry.get("skill-a").is_some(), "skill-a should be in registry");
    assert!(registry.get("skill-b").is_none(), "skill-b excluded by enabled_skills");

    let SkillEntry::Ok(record) = registry.get("skill-a").unwrap() else {
        panic!("expected Ok entry")
    };
    assert_eq!(record.source, SkillSource::Plugin { plugin_name: "my-plugin".to_string() });
}

#[test]
fn plugin_pool_none_enabled_skills_means_all() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "plugins/my-plugin/skills/skill-a/SKILL.md", SKILL_A);
    write_skill(tmp.path(), "plugins/my-plugin/skills/skill-b/SKILL.md", SKILL_B);

    let mut profile = minimal_profile();
    profile.enabled_plugins.insert(
        "my-plugin".to_string(),
        PluginEnablement { enabled: true, enabled_skills: None },
    );

    let registry = SkillRegistry::load(tmp.path(), &profile);
    assert!(registry.get("skill-a").is_some());
    assert!(registry.get("skill-b").is_some());
}

#[test]
fn user_wins_collision() {
    let tmp = tempfile::tempdir().unwrap();
    // User pool has skill-a
    write_skill(tmp.path(), "skills/skill-a/SKILL.md", SKILL_A);
    // Plugin pool also has skill-a (different body)
    let plugin_skill_a = "---\nname: skill-a\ndescription: Plugin version\n---\nplugin-body\n";
    write_skill(tmp.path(), "plugins/my-plugin/skills/skill-a/SKILL.md", plugin_skill_a);

    let mut profile = minimal_profile();
    profile.skills = vec!["skill-a".to_string()];
    profile.enabled_plugins.insert(
        "my-plugin".to_string(),
        PluginEnablement { enabled: true, enabled_skills: None },
    );

    let registry = SkillRegistry::load(tmp.path(), &profile);
    let SkillEntry::Ok(record) = registry.get("skill-a").unwrap() else {
        panic!("expected Ok entry")
    };
    assert_eq!(record.source, SkillSource::User, "user-pool skill must win collision");
    assert_eq!(record.description, "Skill A");
}

#[test]
fn load_error_entry_surfacing() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/bad-skill/SKILL.md", SKILL_BAD);

    let mut profile = minimal_profile();
    profile.skills = vec!["bad-skill".to_string()];

    let registry = SkillRegistry::load(tmp.path(), &profile);
    let entry = registry.get("bad-skill").expect("bad-skill should be present even on parse error");
    assert!(matches!(entry, SkillEntry::Err(_)), "failed skill should be SkillEntry::Err");
}

#[test]
fn builtin_pool_parses_without_filesystem_io() {
    // Construct-only: load_builtin_pool() takes no arguments and touches no
    // filesystem — content is compiled in via include_str!. Proves the
    // embedded create-workflow.md is well-formed frontmatter+body.
    let entries = load_builtin_pool();
    assert_eq!(entries.len(), 1);
    let (name, entry) = &entries[0];
    assert_eq!(name, "create-workflow");
    let SkillEntry::Ok(record) = entry else { panic!("expected Ok entry, got {entry:?}") };
    assert_eq!(record.name, "create-workflow");
    assert_eq!(record.source, SkillSource::BuiltIn);
    assert!(!record.description.is_empty());
    assert!(!record.body.is_empty());
}

#[test]
fn builtin_skill_present_regardless_of_empty_allowlist() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = minimal_profile();
    assert!(profile.skills.is_empty());
    assert!(profile.enabled_plugins.is_empty());

    let registry = SkillRegistry::load(tmp.path(), &profile);
    let entry = registry.get("create-workflow").expect("built-in skill should be present");
    let SkillEntry::Ok(record) = entry else { panic!("expected Ok entry") };
    assert_eq!(record.source, SkillSource::BuiltIn);
}

#[test]
fn user_pool_wins_over_builtin_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let user_override = "---\nname: create-workflow\ndescription: User override\n---\nuser body\n";
    write_skill(tmp.path(), "skills/create-workflow/SKILL.md", user_override);

    let mut profile = minimal_profile();
    profile.skills = vec!["create-workflow".to_string()];

    let registry = SkillRegistry::load(tmp.path(), &profile);
    let SkillEntry::Ok(record) = registry.get("create-workflow").unwrap() else {
        panic!("expected Ok entry")
    };
    assert_eq!(record.source, SkillSource::User, "user-pool skill must win over built-in");
    assert_eq!(record.description, "User override");
}

#[test]
fn empty_profile_produces_only_builtin_entry() {
    // With no user skills and no enabled plugins, the always-on built-in
    // pool (see `builtin_skill_present_regardless_of_empty_allowlist`) is
    // the registry's only contributor.
    let tmp = tempfile::tempdir().unwrap();
    let profile = minimal_profile();
    let registry = SkillRegistry::load(tmp.path(), &profile);
    assert_eq!(registry.all_visible().count(), 1);
    let names: Vec<_> = registry.all_visible().map(|(name, _)| name).collect();
    assert_eq!(names, vec!["create-workflow"]);
}

#[test]
fn all_visible_yields_insertion_order() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/skill-a/SKILL.md", SKILL_A);
    write_skill(tmp.path(), "skills/skill-b/SKILL.md", SKILL_B);

    let mut profile = minimal_profile();
    profile.skills = vec!["skill-a".to_string(), "skill-b".to_string()];

    let registry = SkillRegistry::load(tmp.path(), &profile);
    let names: Vec<_> = registry
        .all_visible()
        .filter_map(|(_, e)| {
            if let SkillEntry::Ok(r) = e { Some(r.name.as_str()) } else { None }
        })
        .collect();
    // user pool (skill-a, skill-b) first, then the always-on built-in pool
    // (create-workflow) last — see `SkillRegistry::load`'s precedence order.
    assert_eq!(names, vec!["skill-a", "skill-b", "create-workflow"]);
}

#[test]
fn get_returns_none_for_absent_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = minimal_profile();
    let registry = SkillRegistry::load(tmp.path(), &profile);
    assert!(registry.get("nonexistent").is_none());
}

// ─── trust gate: set_disable_model_invocation ──────────────────────────────

#[test]
fn set_disable_model_invocation_inserts_key_when_absent() {
    let body = "---\nname: foo\ndescription: Foo\n---\ndo the thing\n";
    let rewritten = set_disable_model_invocation(body, true).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert!(parsed.disable_model_invocation);
    assert_eq!(parsed.name, "foo");
    assert_eq!(parsed.body, "do the thing\n");
}

#[test]
fn set_disable_model_invocation_overrides_existing_false_value() {
    // The model's own claim of `disable-model-invocation: false` must not
    // survive the gate forcing it back to true.
    let body = "---\nname: foo\ndescription: Foo\ndisable-model-invocation: false\n---\nbody\n";
    let rewritten = set_disable_model_invocation(body, true).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert!(parsed.disable_model_invocation, "gate value must win over the body's own claim");
}

#[test]
fn set_disable_model_invocation_can_clear_the_flag() {
    let body = "---\nname: foo\ndescription: Foo\ndisable-model-invocation: true\n---\nbody\n";
    let rewritten = set_disable_model_invocation(body, false).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert!(!parsed.disable_model_invocation);
}

#[test]
fn set_disable_model_invocation_preserves_unrelated_frontmatter_keys() {
    let body = "---\nname: foo\ndescription: Foo\nallowed-tools:\n  - Read\n  - Grep\nmodel: claude-haiku-4-5\n---\nbody\n";
    let rewritten = set_disable_model_invocation(body, true).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.allowed_tools, vec!["Read".to_string(), "Grep".to_string()]);
    assert_eq!(parsed.model.as_deref(), Some("claude-haiku-4-5"));
    assert!(parsed.disable_model_invocation);
}

#[test]
fn set_disable_model_invocation_rejects_malformed_frontmatter() {
    let err = set_disable_model_invocation("no frontmatter here", true).unwrap_err();
    assert!(matches!(err, FrontmatterError::ParseError { .. }));
}

// ─── distillation provenance: set_distilled_origin ─────────────────────────

#[test]
fn set_distilled_origin_inserts_key_when_absent() {
    let body = "---\nname: foo\ndescription: Foo\n---\ndo the thing\n";
    let rewritten = super::frontmatter::set_distilled_origin(body).unwrap();

    // `origin` is not a modeled field yet — round-trips through
    // `parse_frontmatter` unchanged (silently ignored) rather than erroring.
    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.name, "foo");
    assert!(rewritten.contains("origin: distilled"));
}

#[test]
fn set_distilled_origin_preserves_unrelated_frontmatter_keys() {
    let body = "---\nname: foo\ndescription: Foo\ndisable-model-invocation: true\n---\nbody\n";
    let rewritten = super::frontmatter::set_distilled_origin(body).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert!(parsed.disable_model_invocation);
    assert!(rewritten.contains("origin: distilled"));
}

#[test]
fn set_distilled_origin_composes_with_the_disable_invocation_gate() {
    let body = "---\nname: foo\ndescription: Foo\n---\nbody\n";
    let staged = set_disable_model_invocation(body, true).unwrap();
    let marked = super::frontmatter::set_distilled_origin(&staged).unwrap();

    let parsed = parse_frontmatter(&marked).unwrap();
    assert!(parsed.disable_model_invocation);
    assert!(marked.contains("origin: distilled"));
}

// ─── Skill review surface: set_description / set_body ──────────────────────

#[test]
fn set_description_inserts_key_when_absent() {
    let body = "---\nname: foo\n---\ndo the thing\n";
    let rewritten = set_description(body, "A better description").unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.description, "A better description");
    assert_eq!(parsed.body, "do the thing\n");
}

#[test]
fn set_description_overrides_existing_value() {
    let body = "---\nname: foo\ndescription: Old\n---\nbody\n";
    let rewritten = set_description(body, "New").unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.description, "New");
}

#[test]
fn set_description_preserves_unrelated_frontmatter_keys() {
    let body = "---\nname: foo\ndescription: Old\ndisable-model-invocation: true\n---\nbody\n";
    let rewritten = set_description(body, "New").unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.description, "New");
    assert!(parsed.disable_model_invocation);
}

#[test]
fn set_body_replaces_body_and_leaves_frontmatter_untouched() {
    let content = "---\nname: foo\ndescription: Foo\ndisable-model-invocation: true\n---\nold body\n";
    let rewritten = set_body(content, "new body\n").unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.body, "new body\n");
    assert_eq!(parsed.name, "foo");
    assert!(parsed.disable_model_invocation);
}

#[test]
fn set_body_rejects_malformed_frontmatter() {
    let err = set_body("no frontmatter here", "new body").unwrap_err();
    assert!(matches!(err, FrontmatterError::ParseError { .. }));
}

// ─── provenance + versioning metadata ──────────────────────────────────────

#[test]
fn skill_without_version_or_distilled_from_loads_with_back_compat_defaults() {
    // A skill written before the versioning field existed has neither key in
    // its frontmatter.
    let content = "---\nname: legacy-skill\ndescription: Predates versioning\n---\nbody\n";
    let record = parse_frontmatter(content).unwrap();
    assert_eq!(record.version, 1, "a never-versioned skill loads as version 1");
    assert!(record.distilled_from.is_empty());
}

#[test]
fn round_trips_a_skill_carrying_provenance_and_version() {
    let body = "---\nname: my-skill\ndescription: Does a thing\n---\ndo the thing\n";
    let with_origin = super::frontmatter::set_distilled_origin(body).unwrap();
    let with_sources = super::frontmatter::set_distilled_from(
        &with_origin,
        &["cand-1".to_string(), "cand-2".to_string()],
    )
    .unwrap();
    let versioned = super::frontmatter::set_version(&with_sources, 3).unwrap();

    let parsed = parse_frontmatter(&versioned).unwrap();
    assert_eq!(parsed.name, "my-skill");
    assert_eq!(parsed.provenance, super::SkillProvenance::Distilled);
    assert_eq!(parsed.distilled_from, vec!["cand-1".to_string(), "cand-2".to_string()]);
    assert_eq!(parsed.version, 3);
    assert_eq!(parsed.body, "do the thing\n", "the body must survive every stamping pass unchanged");

    // Round-trip again through the same setters (idempotent re-stamping) to
    // confirm nothing about the shape degrades on a second pass.
    let restamped = super::frontmatter::set_version(&versioned, 4).unwrap();
    let reparsed = parse_frontmatter(&restamped).unwrap();
    assert_eq!(reparsed.version, 4);
    assert_eq!(reparsed.distilled_from, vec!["cand-1".to_string(), "cand-2".to_string()]);
    assert_eq!(reparsed.provenance, super::SkillProvenance::Distilled);
}

#[test]
fn set_version_inserts_key_when_absent() {
    let body = "---\nname: foo\ndescription: Foo\n---\nbody\n";
    let rewritten = super::frontmatter::set_version(body, 2).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.version, 2);
    assert!(rewritten.contains("version: 2"));
}

#[test]
fn set_version_overwrites_an_existing_value() {
    let body = "---\nname: foo\ndescription: Foo\nversion: 1\n---\nbody\n";
    let rewritten = super::frontmatter::set_version(body, 5).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.version, 5);
}

#[test]
fn set_version_preserves_unrelated_frontmatter_keys() {
    let body = "---\nname: foo\ndescription: Foo\nallowed-tools:\n  - Read\n---\nbody\n";
    let rewritten = super::frontmatter::set_version(body, 2).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.allowed_tools, vec!["Read".to_string()]);
    assert_eq!(parsed.version, 2);
}

#[test]
fn set_distilled_from_inserts_key_when_absent() {
    let body = "---\nname: foo\ndescription: Foo\n---\nbody\n";
    let ids = vec!["cand-a".to_string(), "cand-b".to_string()];
    let rewritten = super::frontmatter::set_distilled_from(body, &ids).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.distilled_from, ids);
}

#[test]
fn set_distilled_from_replaces_a_prior_list() {
    let body = "---\nname: foo\ndescription: Foo\ndistilled-from:\n  - old-cand\n---\nbody\n";
    let ids = vec!["new-cand".to_string()];
    let rewritten = super::frontmatter::set_distilled_from(body, &ids).unwrap();

    let parsed = parse_frontmatter(&rewritten).unwrap();
    assert_eq!(parsed.distilled_from, ids);
}

// ─── Search-index foundation: skill adapter ───────────────────────────────

#[test]
fn skill_index_records_maps_ok_entries_to_global_scope() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/skill-a/SKILL.md", SKILL_A);

    let mut profile = minimal_profile();
    profile.skills = vec!["skill-a".to_string()];
    let registry = SkillRegistry::load(tmp.path(), &profile);

    // The always-on built-in pool (`create-workflow`) also contributes an Ok
    // entry to every registry, so look up by id rather than assuming
    // `skill-a` is the only record.
    let records = skill_index_records(&registry);
    let record = records.iter().find(|r| r.id == "skill-a").expect("skill-a should be indexed");
    assert_eq!(record.scope, IndexScope::Global);
    assert_eq!(record.artifact, ArtifactKind::Skill);
    assert!(record.text.contains("skill-a"));
    assert!(record.text.contains("Skill A"));
}

#[test]
fn skill_index_records_skips_load_error_entries() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/skill-a/SKILL.md", SKILL_A);
    write_skill(tmp.path(), "skills/bad-skill/SKILL.md", SKILL_BAD);

    let mut profile = minimal_profile();
    profile.skills = vec!["skill-a".to_string(), "bad-skill".to_string()];
    let registry = SkillRegistry::load(tmp.path(), &profile);

    let records = skill_index_records(&registry);
    assert!(
        records.iter().all(|r| r.id != "bad-skill"),
        "a skill that failed to parse must not produce a search row"
    );
    assert!(records.iter().any(|r| r.id == "skill-a"), "skill-a should be indexed");
}

#[test]
fn skill_index_records_includes_when_to_use_hint() {
    let content = "---\nname: my-skill\ndescription: Does a thing\nwhen-to-use: When the user asks for the thing\n---\nbody\n";
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/my-skill/SKILL.md", content);

    let mut profile = minimal_profile();
    profile.skills = vec!["my-skill".to_string()];
    let registry = SkillRegistry::load(tmp.path(), &profile);

    let records = skill_index_records(&registry);
    let record = records.iter().find(|r| r.id == "my-skill").expect("my-skill should be indexed");
    assert!(record.text.contains("When the user asks for the thing"));
}

// Deliberately disjoint vocabulary (no shared words, including in the
// filler text) — query terms are OR'd together (see
// `ao_search_index::query::build_match_expression`), so two fixtures that
// share a word like "skill" would make every query below ambiguous about
// which fixture actually matched.
const SKILL_ALPHA: &str = "---\nname: alpha-widget\ndescription: Calibrates the alpha widget\n---\nbody\n";
const SKILL_BETA: &str = "---\nname: beta-harbor\ndescription: Inspects the beta harbor gate\n---\nbody\n";

#[tokio::test]
async fn reindex_skills_populates_the_index_under_global_scope() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/alpha-widget/SKILL.md", SKILL_ALPHA);
    write_skill(tmp.path(), "skills/beta-harbor/SKILL.md", SKILL_BETA);

    let mut profile = minimal_profile();
    profile.skills = vec!["alpha-widget".to_string(), "beta-harbor".to_string()];
    let registry = SkillRegistry::load(tmp.path(), &profile);

    let index = SearchIndex::open_in_memory().unwrap();
    reindex_skills(&index, &registry).await.unwrap();

    let hits = index
        .query("calibrates".into(), SearchFilter::new().with_artifact(ArtifactKind::Skill), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "alpha-widget");

    let scoped_hits = index
        .query(
            "calibrates".into(),
            SearchFilter::new().with_scope(IndexScope::Global).with_artifact(ArtifactKind::Skill),
            10,
        )
        .await
        .unwrap();
    assert_eq!(scoped_hits.len(), 1);
}

#[tokio::test]
async fn reindex_skills_removes_rows_for_skills_no_longer_present() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/alpha-widget/SKILL.md", SKILL_ALPHA);
    write_skill(tmp.path(), "skills/beta-harbor/SKILL.md", SKILL_BETA);

    let mut profile = minimal_profile();
    profile.skills = vec!["alpha-widget".to_string(), "beta-harbor".to_string()];
    let registry = SkillRegistry::load(tmp.path(), &profile);

    let index = SearchIndex::open_in_memory().unwrap();
    reindex_skills(&index, &registry).await.unwrap();

    // beta-harbor is retired from the profile; a fresh load + reindex must
    // drop its row rather than leaving it stale.
    let mut narrowed_profile = minimal_profile();
    narrowed_profile.skills = vec!["alpha-widget".to_string()];
    let narrowed_registry = SkillRegistry::load(tmp.path(), &narrowed_profile);
    reindex_skills(&index, &narrowed_registry).await.unwrap();

    let hits = index
        .query("harbor".into(), SearchFilter::new(), 10)
        .await
        .unwrap();
    assert!(hits.is_empty());
    let remaining = index.query("calibrates".into(), SearchFilter::new(), 10).await.unwrap();
    assert_eq!(remaining.len(), 1);
}

#[tokio::test]
async fn reindex_skills_leaves_memory_rows_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "skills/skill-a/SKILL.md", SKILL_A);

    let mut profile = minimal_profile();
    profile.skills = vec!["skill-a".to_string()];
    let registry = SkillRegistry::load(tmp.path(), &profile);

    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(ao_search_index::IndexRecord {
            id: "mem-1".to_string(),
            scope: IndexScope::Global,
            artifact: ArtifactKind::Memory,
            text: "an unrelated memory entry".to_string(),
        })
        .await
        .unwrap();

    reindex_skills(&index, &registry).await.unwrap();

    let hits = index
        .query("unrelated memory entry".into(), SearchFilter::new(), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "reindexing skills must not disturb memory rows");
}
