/// Integration tests for skills HTTP routes with user-pool scoping.
///
/// Coverage:
/// (a) write_skill → list returns only the new skill for this agent
/// (b) import_folder → list shows it for importing agent, NOT for another agent
/// (c) delete removes from profile but file remains in pool
/// (d) patch enabled=true with skill not in pool → 404
/// (e) patch auto_sync=true → 400
/// (f) plugin-pool skill appears in list with source="plugin"
/// (g) refresh rescans pool, returns agent-scoped subset
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ao_engine::skills::{SkillDto, SkillSource};
use ao_engine::AppState;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, PluginEnablement, ProviderConfig,
};
use ao_server::routes::build_router;

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_profile(id: &str) -> AgentProfile {
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
        enabled_plugins: HashMap::new(),
        runner_mode: Default::default(),
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

async fn setup() -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(vec![]);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };
    (build_router(state), tmp)
}

async fn create_agent(router: &axum::Router, profile: &AgentProfile) {
    let body = serde_json::to_string(profile).unwrap();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "create_agent failed");
}

async fn post_skill(
    router: &axum::Router,
    agent_id: &str,
    title: &str,
    description: &str,
    content: &str,
) -> StatusCode {
    let body = serde_json::json!({
        "title": title,
        "description": description,
        "content": content,
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{agent_id}/skills"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

async fn list_skills(router: &axum::Router, agent_id: &str) -> Vec<SkillDto> {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&format!("/agents/{agent_id}/skills"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("decode SkillDto list")
}

async fn delete_skill(router: &axum::Router, agent_id: &str, skill_id: &str) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(&format!("/agents/{agent_id}/skills/{skill_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn patch_skill(
    router: &axum::Router,
    agent_id: &str,
    skill_id: &str,
    body: serde_json::Value,
) -> (StatusCode, Vec<u8>) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(&format!("/agents/{agent_id}/skills/{skill_id}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, bytes)
}

// ── (a) write_skill → list returns only the new skill for this agent ────────

#[tokio::test]
async fn write_skill_appears_in_list_for_that_agent() {
    let (router, _tmp) = setup().await;
    let agent_id = "rt-write-a";
    create_agent(&router, &make_profile(agent_id)).await;

    let status = post_skill(&router, agent_id, "My Skill", "desc", "body").await;
    assert_eq!(status, StatusCode::CREATED);

    let skills = list_skills(&router, agent_id).await;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "my-skill");
    assert!(
        matches!(skills[0].source, SkillSource::User),
        "skill should be user-sourced"
    );
}

#[tokio::test]
async fn write_skill_does_not_appear_in_other_agents_list() {
    let (router, _tmp) = setup().await;
    let a1 = "rt-write-b1";
    let a2 = "rt-write-b2";
    create_agent(&router, &make_profile(a1)).await;
    create_agent(&router, &make_profile(a2)).await;

    post_skill(&router, a1, "Shared Skill", "desc", "body").await;

    let skills_a1 = list_skills(&router, a1).await;
    let skills_a2 = list_skills(&router, a2).await;

    assert_eq!(skills_a1.len(), 1, "a1 should see the skill");
    assert_eq!(skills_a2.len(), 0, "a2 should NOT see a1's skill");
}

// ── (b) import_folder → per-agent scoping ───────────────────────────────────

#[tokio::test]
async fn import_folder_shows_for_importing_agent_not_other() {
    let (router, tmp) = setup().await;
    let a1 = "rt-import-c1";
    let a2 = "rt-import-c2";
    create_agent(&router, &make_profile(a1)).await;
    create_agent(&router, &make_profile(a2)).await;

    // Create a source folder with a SKILL.md
    let src_dir = tmp.path().join("my-bundle");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("SKILL.md"),
        "---\nname: my-bundle\ndescription: A bundle skill\n---\n\nbody",
    )
    .unwrap();

    let body = serde_json::json!({"src_path": src_dir.to_string_lossy()});
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{a1}/skills/import-folder"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let skills_a1 = list_skills(&router, a1).await;
    let skills_a2 = list_skills(&router, a2).await;

    assert_eq!(skills_a1.len(), 1, "a1 should see imported skill");
    assert_eq!(skills_a2.len(), 0, "a2 should not see a1's import");
}

// ── (c) delete removes from profile but file remains in pool ────────────────

#[tokio::test]
async fn delete_removes_from_profile_but_file_stays_in_pool() {
    let (router, tmp) = setup().await;
    let a1 = "rt-delete-d1";
    let a2 = "rt-delete-d2";
    create_agent(&router, &make_profile(a1)).await;
    create_agent(&router, &make_profile(a2)).await;

    // a1 writes a skill
    post_skill(&router, a1, "Common Skill", "desc", "body").await;

    // a2 manually patches the skill into its profile via enabled=true
    let (status, _) = patch_skill(
        &router,
        a2,
        "common-skill",
        serde_json::json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a2 should be able to enable the shared skill");

    // Now a1 deletes (removes from profile)
    let del_status = delete_skill(&router, a1, "common-skill").await;
    assert_eq!(del_status, StatusCode::NO_CONTENT);

    // a1 no longer sees it
    let skills_a1 = list_skills(&router, a1).await;
    assert!(
        skills_a1.iter().all(|s| s.id != "common-skill"),
        "a1 should not see the deleted skill"
    );

    // a2 still sees it (file is still in pool)
    let skills_a2 = list_skills(&router, a2).await;
    assert!(
        skills_a2.iter().any(|s| s.id == "common-skill"),
        "a2 should still see the skill (file remains in pool)"
    );

    // The file still exists on disk
    let pool_skill_file = tmp.path().join("skills").join("common-skill").join("SKILL.md");
    assert!(pool_skill_file.exists(), "SKILL.md must still exist in pool after soft-delete");
}

// ── (d) patch enabled=true with skill not in pool → 404 ────────────────────

#[tokio::test]
async fn patch_enabled_true_with_skill_not_in_pool_returns_404() {
    let (router, _tmp) = setup().await;
    let agent_id = "rt-patch-e";
    create_agent(&router, &make_profile(agent_id)).await;

    let (status, _) = patch_skill(
        &router,
        agent_id,
        "nonexistent-skill",
        serde_json::json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── (e) patch auto_sync=true → 400 ─────────────────────────────────────────

#[tokio::test]
async fn patch_auto_sync_true_returns_400() {
    let (router, _tmp) = setup().await;
    let agent_id = "rt-autosync-f";
    create_agent(&router, &make_profile(agent_id)).await;

    post_skill(&router, agent_id, "My Skill", "desc", "body").await;

    let (status, body) = patch_skill(
        &router,
        agent_id,
        "my-skill",
        serde_json::json!({"auto_sync": true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let error_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        error_body["error"]
            .as_str()
            .unwrap_or("")
            .contains("auto_sync"),
        "error message should mention auto_sync"
    );
}

// ── (f) plugin-pool skill appears with source="plugin" ──────────────────────

#[tokio::test]
async fn plugin_pool_skill_appears_in_list_with_plugin_source() {
    let (router, tmp) = setup().await;
    let agent_id = "rt-plugin-g";

    // Set up a plugin skill in the pool
    let plugin_skill_dir = tmp
        .path()
        .join("plugins")
        .join("my-plugin")
        .join("skills")
        .join("plugin-tip");
    std::fs::create_dir_all(&plugin_skill_dir).unwrap();
    std::fs::write(
        plugin_skill_dir.join("SKILL.md"),
        "---\nname: plugin-tip\ndescription: A plugin tip\n---\nbody",
    )
    .unwrap();

    // Create agent with the plugin enabled
    let mut profile = make_profile(agent_id);
    profile.enabled_plugins.insert(
        "my-plugin".to_string(),
        PluginEnablement {
            enabled: true,
            enabled_skills: None,
        },
    );
    create_agent(&router, &profile).await;

    let skills = list_skills(&router, agent_id).await;
    let plugin_skill = skills
        .iter()
        .find(|s| s.id == "plugin-tip")
        .expect("plugin-tip should be in the list");

    assert!(
        matches!(plugin_skill.source, SkillSource::Plugin),
        "plugin skill should have source=plugin, got {:?}",
        plugin_skill.source
    );
}

// ── (g) patch enabled=false removes from list, patch enabled=true adds back ─

#[tokio::test]
async fn patch_enabled_toggle_adds_and_removes_from_list() {
    let (router, _tmp) = setup().await;
    let a1 = "rt-toggle-h1";
    let a2 = "rt-toggle-h2";
    create_agent(&router, &make_profile(a1)).await;
    create_agent(&router, &make_profile(a2)).await;

    // a1 creates a skill
    post_skill(&router, a1, "Toggle Skill", "desc", "body").await;

    // a2 enables the skill via PATCH
    let (status, _) = patch_skill(
        &router,
        a2,
        "toggle-skill",
        serde_json::json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let skills_a2 = list_skills(&router, a2).await;
    assert!(skills_a2.iter().any(|s| s.id == "toggle-skill"), "a2 should see toggle-skill after enable");

    // a2 disables the skill via PATCH enabled=false
    let (status, _) = patch_skill(
        &router,
        a2,
        "toggle-skill",
        serde_json::json!({"enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let skills_a2 = list_skills(&router, a2).await;
    assert!(
        !skills_a2.iter().any(|s| s.id == "toggle-skill"),
        "a2 should not see toggle-skill after disable"
    );

    // a1 still sees it (its own copy)
    let skills_a1 = list_skills(&router, a1).await;
    assert!(skills_a1.iter().any(|s| s.id == "toggle-skill"), "a1 should still see toggle-skill");
}

// ── delete is idempotent ────────────────────────────────────────────────────

#[tokio::test]
async fn delete_skill_is_idempotent() {
    let (router, _tmp) = setup().await;
    let agent_id = "rt-idem-i";
    create_agent(&router, &make_profile(agent_id)).await;

    // Delete a skill that was never added → 204 (idempotent)
    let status = delete_skill(&router, agent_id, "never-existed").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ── refresh returns agent-scoped subset ────────────────────────────────────

#[tokio::test]
async fn refresh_skills_returns_agent_scoped_subset() {
    let (router, _tmp) = setup().await;
    let a1 = "rt-refresh-j1";
    let a2 = "rt-refresh-j2";
    create_agent(&router, &make_profile(a1)).await;
    create_agent(&router, &make_profile(a2)).await;

    post_skill(&router, a1, "Skill One", "desc", "body").await;
    post_skill(&router, a2, "Skill Two", "desc", "body").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{a1}/skills/refresh"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let skills: Vec<SkillDto> = serde_json::from_slice(&bytes).unwrap();

    // a1's refresh shows only a1's skill, not a2's
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "skill-one");
}
