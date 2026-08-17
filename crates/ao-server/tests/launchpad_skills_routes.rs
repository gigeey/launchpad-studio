/// Integration tests for the convention-folder ("launchpad-skills") routes.
///
/// Mirrors the harness shape of `tests/skills_routes.rs` (tempdir data_root
/// via `LAUNCHPAD_STUDIO_DATA_DIR`, `axum::Router` + `tower::ServiceExt::oneshot`)
/// but adds a second tempdir standing in for a focused project's working
/// directory, since project-scoped convention skills are read from
/// `<focus_path>/.launchpad/skills` rather than the pool.
///
/// Coverage:
/// (a) GET /skills/launchpad/global returns dropped folders
/// (b) POST /agents/{id}/launchpad-skills/global persists to the profile
/// (c) POST /agents/{id}/launchpad-skills/project persists under project_key,
///     and dropping the last enabled skill removes the empty map entry
/// (d) GET /skills/launchpad/project resolves project_key even when the
///     `.launchpad/skills` dir is absent, and returns an empty/omitted
///     project_key when focus_path is unset
/// (e) POST /skills/launchpad/promote copies the folder and returns it
/// (f) promoting into an existing global name refuses with 409 and leaves
///     the existing global folder untouched
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ao_engine::AppState;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
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

async fn get_agent(router: &axum::Router, agent_id: &str) -> AgentProfile {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&format!("/agents/{agent_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("decode AgentProfile")
}

async fn request_json(
    router: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("decode JSON body")
    };
    (status, json)
}

fn write_skill_folder(skills_dir: &std::path::Path, name: &str, description: &str) {
    let dir = skills_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\nbody"),
    )
    .unwrap();
}

// ── (a) GET /skills/launchpad/global returns dropped folders ────────────────

#[tokio::test]
async fn list_global_skills_returns_dropped_folders() {
    let (router, tmp) = setup().await;

    let global_skills_dir = tmp.path().join(".launchpad").join("skills");
    write_skill_folder(&global_skills_dir, "global-one", "A dropped global skill");

    let (status, body) = request_json(&router, Method::GET, "/skills/launchpad/global", None).await;
    assert_eq!(status, StatusCode::OK);

    let skills = body["skills"].as_array().expect("skills array");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"], "global-one");
    assert_eq!(skills[0]["description"], "A dropped global skill");
}

#[tokio::test]
async fn list_global_skills_empty_when_dir_missing() {
    let (router, _tmp) = setup().await;

    let (status, body) = request_json(&router, Method::GET, "/skills/launchpad/global", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skills"].as_array().unwrap().len(), 0);
}

// ── (b) POST /agents/{id}/launchpad-skills/global persists to profile ───────

#[tokio::test]
async fn enable_global_skill_persists_to_profile() {
    let (router, _tmp) = setup().await;
    let agent_id = "lp-global-a";
    create_agent(&router, &make_profile(agent_id)).await;

    let (status, body) = request_json(
        &router,
        Method::POST,
        &format!("/agents/{agent_id}/launchpad-skills/global"),
        Some(serde_json::json!({"skill_name": "global-one", "enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skill_name"], "global-one");
    assert_eq!(body["enabled"], true);

    let profile = get_agent(&router, agent_id).await;
    assert_eq!(
        profile.enabled_launchpad_global_skills,
        Some(vec!["global-one".to_string()])
    );

    // Disabling removes it and drops back to an empty/None subset.
    let (status, _) = request_json(
        &router,
        Method::POST,
        &format!("/agents/{agent_id}/launchpad-skills/global"),
        Some(serde_json::json!({"skill_name": "global-one", "enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let profile = get_agent(&router, agent_id).await;
    assert!(
        profile
            .enabled_launchpad_global_skills
            .unwrap_or_default()
            .is_empty(),
        "disabling the only enabled global skill should leave none enabled"
    );
}

// ── (c) POST /agents/{id}/launchpad-skills/project persists under project_key ─

#[tokio::test]
async fn enable_project_skill_persists_under_project_key_and_drops_when_empty() {
    let (router, _tmp) = setup().await;
    let project_tmp = tempfile::tempdir().expect("project tempdir");
    let focus_path = project_tmp.path().to_string_lossy().to_string();

    let project_skills_dir = project_tmp.path().join(".launchpad").join("skills");
    write_skill_folder(&project_skills_dir, "proj-one", "A project skill");

    let agent_id = "lp-project-b";
    create_agent(&router, &make_profile(agent_id)).await;

    let (status, list_body) = request_json(
        &router,
        Method::GET,
        &format!(
            "/skills/launchpad/project?focus_path={}",
            urlencoding_encode(&focus_path)
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let project_key = list_body["project_key"]
        .as_str()
        .expect("project_key present")
        .to_string();
    assert!(!project_key.is_empty());
    assert_eq!(list_body["skills"].as_array().unwrap().len(), 1);

    let (status, enable_body) = request_json(
        &router,
        Method::POST,
        &format!("/agents/{agent_id}/launchpad-skills/project"),
        Some(serde_json::json!({
            "project_key": project_key,
            "skill_name": "proj-one",
            "enabled": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(enable_body["project_key"], project_key);
    assert_eq!(enable_body["skill_name"], "proj-one");
    assert_eq!(enable_body["enabled"], true);

    let profile = get_agent(&router, agent_id).await;
    assert_eq!(
        profile.enabled_launchpad_project_skills.get(&project_key),
        Some(&vec!["proj-one".to_string()])
    );

    // Disabling the only enabled project skill drops the empty map entry
    // entirely rather than leaving a stray empty Vec.
    let (status, _) = request_json(
        &router,
        Method::POST,
        &format!("/agents/{agent_id}/launchpad-skills/project"),
        Some(serde_json::json!({
            "project_key": project_key,
            "skill_name": "proj-one",
            "enabled": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let profile = get_agent(&router, agent_id).await;
    assert!(
        !profile
            .enabled_launchpad_project_skills
            .contains_key(&project_key),
        "disabling the last enabled project skill must drop the map key, not leave an empty Vec"
    );
}

// ── (d) GET /skills/launchpad/project edge cases ─────────────────────────────

#[tokio::test]
async fn list_project_skills_resolves_key_even_when_dir_absent() {
    let (router, _tmp) = setup().await;
    let project_tmp = tempfile::tempdir().expect("project tempdir");
    let focus_path = project_tmp.path().to_string_lossy().to_string();

    // No `.launchpad/skills` dir created under project_tmp.
    let (status, body) = request_json(
        &router,
        Method::GET,
        &format!(
            "/skills/launchpad/project?focus_path={}",
            urlencoding_encode(&focus_path)
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skills"].as_array().unwrap().len(), 0);
    assert!(
        body["project_key"].as_str().is_some_and(|k| !k.is_empty()),
        "project_key should still resolve when the convention dir is missing"
    );
}

#[tokio::test]
async fn list_project_skills_omits_project_key_when_focus_path_unset() {
    let (router, _tmp) = setup().await;

    let (status, body) =
        request_json(&router, Method::GET, "/skills/launchpad/project", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skills"].as_array().unwrap().len(), 0);
    let project_key = body.get("project_key").and_then(|v| v.as_str()).unwrap_or("");
    assert!(project_key.is_empty(), "project_key must be empty/omitted when focus_path is unset");
}

// ── (e) POST /skills/launchpad/promote copies the folder and returns it ─────

#[tokio::test]
async fn promote_copies_project_skill_into_global_root() {
    let (router, tmp) = setup().await;
    let project_tmp = tempfile::tempdir().expect("project tempdir");
    let focus_path = project_tmp.path().to_string_lossy().to_string();

    let project_skills_dir = project_tmp.path().join(".launchpad").join("skills");
    write_skill_folder(&project_skills_dir, "cool-skill", "A cool project skill");

    let (status, body) = request_json(
        &router,
        Method::POST,
        "/skills/launchpad/promote",
        Some(serde_json::json!({"focus_path": focus_path, "skill_name": "cool-skill"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["promoted"], "cool-skill");

    let copied = tmp
        .path()
        .join(".launchpad")
        .join("skills")
        .join("cool-skill")
        .join("SKILL.md");
    assert!(copied.exists(), "promote must copy the SKILL.md into the global root");
    let content = std::fs::read_to_string(&copied).unwrap();
    assert!(content.contains("A cool project skill"));

    // It now also shows up in the global list.
    let (status, list_body) = request_json(&router, Method::GET, "/skills/launchpad/global", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(list_body["skills"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"] == "cool-skill"));
}

// ── (f) promote into an existing global name → 409, no overwrite ────────────

#[tokio::test]
async fn promote_into_existing_global_name_returns_409_and_does_not_overwrite() {
    let (router, tmp) = setup().await;

    let global_skills_dir = tmp.path().join(".launchpad").join("skills");
    write_skill_folder(&global_skills_dir, "cool-skill", "Original global version");

    let project_tmp = tempfile::tempdir().expect("project tempdir");
    let focus_path = project_tmp.path().to_string_lossy().to_string();
    let project_skills_dir = project_tmp.path().join(".launchpad").join("skills");
    write_skill_folder(&project_skills_dir, "cool-skill", "Different project version");

    let (status, _body) = request_json(
        &router,
        Method::POST,
        "/skills/launchpad/promote",
        Some(serde_json::json!({"focus_path": focus_path, "skill_name": "cool-skill"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let global_content = std::fs::read_to_string(
        tmp.path()
            .join(".launchpad")
            .join("skills")
            .join("cool-skill")
            .join("SKILL.md"),
    )
    .unwrap();
    assert!(
        global_content.contains("Original global version"),
        "refused promote must not overwrite the existing global skill"
    );
}

/// Minimal percent-encoding for a query-string value (temp-dir paths can
/// contain characters like spaces on some platforms). Avoids pulling in a
/// dependency just for test fixtures.
fn urlencoding_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}
