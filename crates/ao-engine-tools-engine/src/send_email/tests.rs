use std::collections::HashMap;
use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, RunnerContext};
use ao_engine_tools_provider_config::ChannelSecretStore;
use ao_persistence::paths::DataRoot;
use ao_persistence::profiles::AgentProfileStore;
use ao_protocol::agent::{
    AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig, CliProviderConfig, InputMode, OutputFormat,
    ProviderConfig,
};
use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Mutex;

use super::smtp_seam::{OutboundEmail, SendErrorKind, SendOutcome, SmtpSender};
use super::{ensure_re_prefixed, normalize_message_id, resolve_email_binding, SendEmail};

// ─── helpers ─────────────────────────────────────────────────────────────────

fn make_profile(id: &str, channels: Vec<ChannelBinding>) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: "Test Agent".to_string(),
        description: "test agent".to_string(),
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
        native_provider: None,
        thinking: None,
        max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        enabled_plugins: HashMap::new(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        delegates_to: vec![],
        persona: None,
        special_instructions: None,
        legacy_system_prompt: None,
        max_delegation_depth: None,
        channels,
        max_turns: None,
    }
}

fn email_binding(binding_id: &str, enabled: bool, address: &str) -> ChannelBinding {
    ChannelBinding {
        binding_id: binding_id.to_string(),
        kind: ChannelKind::Email,
        enabled,
        bridge_thread_id: None,
        allowed_senders: vec![],
        pending_pairing_code: None,
        kind_config: ChannelKindConfig::Email {
            address: address.to_string(),
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            poll_secs: 15,
            require_auth_results: true,
        },
    }
}

async fn setup(tmp: &TempDir) -> (Arc<AgentProfileStore>, Arc<ChannelSecretStore>) {
    let data_root = DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(AgentProfileStore::new(data_root));
    let secret_store = Arc::new(ChannelSecretStore::new_with_file_fallback(tmp.path().to_path_buf()));
    (store, secret_store)
}

fn make_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("session", "agent-1", std::env::temp_dir())
}

/// Records the last send it was asked to perform and returns a scripted
/// outcome, so `invoke` tests can assert on exactly what reached the SMTP
/// seam without any network I/O.
struct FakeSmtpSender {
    outcome: SendOutcome,
    last_call: Mutex<Option<(String, u16, String)>>,
}

impl FakeSmtpSender {
    fn new(outcome: SendOutcome) -> Self {
        Self { outcome, last_call: Mutex::new(None) }
    }
}

#[async_trait]
impl SmtpSender for FakeSmtpSender {
    async fn send(&self, host: &str, port: u16, username: &str, _password: &str, _email: &OutboundEmail) -> SendOutcome {
        *self.last_call.lock().await = Some((host.to_string(), port, username.to_string()));
        self.outcome.clone()
    }
}

// ─── resolve_email_binding ──────────────────────────────────────────────────

#[test]
fn resolve_email_binding_picks_the_single_enabled_binding() {
    let profile = make_profile("a", vec![email_binding("email", true, "agent@example.org")]);
    let binding = resolve_email_binding(&profile, None).expect("resolves");
    assert_eq!(binding.binding_id, "email");
}

#[test]
fn resolve_email_binding_errors_when_none_enabled() {
    let profile = make_profile("a", vec![email_binding("email", false, "agent@example.org")]);
    assert!(resolve_email_binding(&profile, None).is_err());
}

#[test]
fn resolve_email_binding_errors_when_ambiguous() {
    let profile = make_profile(
        "a",
        vec![
            email_binding("email-1", true, "one@example.org"),
            email_binding("email-2", true, "two@example.org"),
        ],
    );
    assert!(resolve_email_binding(&profile, None).is_err());
}

#[test]
fn resolve_email_binding_honors_explicit_binding_id() {
    let profile = make_profile(
        "a",
        vec![
            email_binding("email-1", true, "one@example.org"),
            email_binding("email-2", true, "two@example.org"),
        ],
    );
    let binding = resolve_email_binding(&profile, Some("email-2")).expect("resolves");
    assert_eq!(binding.binding_id, "email-2");
}

#[test]
fn resolve_email_binding_errors_on_unknown_explicit_binding_id() {
    let profile = make_profile("a", vec![email_binding("email", true, "agent@example.org")]);
    assert!(resolve_email_binding(&profile, Some("nope")).is_err());
}

// ─── ensure_re_prefixed / normalize_message_id ─────────────────────────────

#[test]
fn ensure_re_prefixed_adds_prefix_when_absent() {
    assert_eq!(ensure_re_prefixed("Hello"), "Re: Hello");
}

#[test]
fn ensure_re_prefixed_does_not_double_prefix() {
    assert_eq!(ensure_re_prefixed("Re: Hello"), "Re: Hello");
    assert_eq!(ensure_re_prefixed("re: Hello"), "re: Hello");
}

#[test]
fn normalize_message_id_wraps_bare_id() {
    assert_eq!(normalize_message_id("abc123@example.com"), "<abc123@example.com>");
}

#[test]
fn normalize_message_id_leaves_already_wrapped_id_unchanged() {
    assert_eq!(normalize_message_id("<abc123@example.com>"), "<abc123@example.com>");
}

// ─── invoke ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn invoke_sends_and_returns_success_with_message_id() {
    let tmp = TempDir::new().unwrap();
    let (store, secret_store) = setup(&tmp).await;
    let profile = make_profile("agent-1", vec![email_binding("email", true, "agent@example.org")]);
    store.create(&profile).await.unwrap();
    secret_store.set("agent-1", "email", ao_engine_tools_provider_config::EMAIL_PASSWORD_SECRET_ROLE, "app-password").unwrap();

    let sender = Arc::new(FakeSmtpSender::new(SendOutcome {
        success: true,
        message_id: Some("<generated@example.org>".to_string()),
        error_kind: None,
        retryable: false,
    }));
    let tool = SendEmail::with_deps_and_sender(store, secret_store, sender.clone());

    let out = tool
        .invoke(
            json!({"to": "user@example.com", "subject": "Hi", "body": "Hello there"}),
            &make_ctx(),
        )
        .await
        .unwrap();

    match out {
        ao_engine_tools_core::ToolOutput::Structured(v) => {
            assert_eq!(v["success"], json!(true));
            assert_eq!(v["message_id"], json!("<generated@example.org>"));
        }
        other => panic!("expected structured success output, got {other:?}"),
    }

    let call = sender.last_call.lock().await.clone().expect("sender was called");
    assert_eq!(call, ("smtp.example.com".to_string(), 587, "agent@example.org".to_string()));
}

#[tokio::test]
async fn invoke_errors_clearly_when_no_password_is_stored() {
    let tmp = TempDir::new().unwrap();
    let (store, secret_store) = setup(&tmp).await;
    let profile = make_profile("agent-1", vec![email_binding("email", true, "agent@example.org")]);
    store.create(&profile).await.unwrap();
    // No password set in the secret store.

    let sender = Arc::new(FakeSmtpSender::new(SendOutcome {
        success: true,
        message_id: None,
        error_kind: None,
        retryable: false,
    }));
    let tool = SendEmail::with_deps_and_sender(store, secret_store, sender.clone());

    let out = tool
        .invoke(json!({"to": "user@example.com", "subject": "Hi", "body": "body"}), &make_ctx())
        .await
        .unwrap();

    match out {
        ao_engine_tools_core::ToolOutput::Error { message, .. } => {
            assert!(message.contains("no password stored"), "unexpected message: {message}");
        }
        other => panic!("expected an error output, got {other:?}"),
    }
    assert!(sender.last_call.lock().await.is_none(), "must not attempt to send without a password");
}

#[tokio::test]
async fn invoke_errors_when_no_enabled_email_binding_exists() {
    let tmp = TempDir::new().unwrap();
    let (store, secret_store) = setup(&tmp).await;
    let profile = make_profile("agent-1", vec![]);
    store.create(&profile).await.unwrap();

    let sender = Arc::new(FakeSmtpSender::new(SendOutcome {
        success: true,
        message_id: None,
        error_kind: None,
        retryable: false,
    }));
    let tool = SendEmail::with_deps_and_sender(store, secret_store, sender);

    let out = tool
        .invoke(json!({"to": "user@example.com", "subject": "Hi", "body": "body"}), &make_ctx())
        .await
        .unwrap();

    match out {
        ao_engine_tools_core::ToolOutput::Error { message, .. } => {
            assert!(message.contains("no enabled Email binding"), "unexpected message: {message}");
        }
        other => panic!("expected an error output, got {other:?}"),
    }
}

#[tokio::test]
async fn invoke_surfaces_retryable_flag_on_failure() {
    let tmp = TempDir::new().unwrap();
    let (store, secret_store) = setup(&tmp).await;
    let profile = make_profile("agent-1", vec![email_binding("email", true, "agent@example.org")]);
    store.create(&profile).await.unwrap();
    secret_store.set("agent-1", "email", ao_engine_tools_provider_config::EMAIL_PASSWORD_SECRET_ROLE, "app-password").unwrap();

    let sender = Arc::new(FakeSmtpSender::new(SendOutcome {
        success: false,
        message_id: None,
        error_kind: Some(SendErrorKind::Transient),
        retryable: true,
    }));
    let tool = SendEmail::with_deps_and_sender(store, secret_store, sender);

    let out = tool
        .invoke(json!({"to": "user@example.com", "subject": "Hi", "body": "body"}), &make_ctx())
        .await
        .unwrap();

    match out {
        ao_engine_tools_core::ToolOutput::Error { recoverable, .. } => assert!(recoverable, "transient failure must be retryable"),
        other => panic!("expected an error output, got {other:?}"),
    }
}

#[tokio::test]
async fn invoke_prefixes_subject_with_re_when_replying() {
    let tmp = TempDir::new().unwrap();
    let (store, secret_store) = setup(&tmp).await;
    let profile = make_profile("agent-1", vec![email_binding("email", true, "agent@example.org")]);
    store.create(&profile).await.unwrap();
    secret_store.set("agent-1", "email", ao_engine_tools_provider_config::EMAIL_PASSWORD_SECRET_ROLE, "app-password").unwrap();

    struct CapturingSender {
        captured: Mutex<Option<OutboundEmail>>,
    }
    #[async_trait]
    impl SmtpSender for CapturingSender {
        async fn send(&self, _host: &str, _port: u16, _username: &str, _password: &str, email: &OutboundEmail) -> SendOutcome {
            *self.captured.lock().await = Some(email.clone());
            SendOutcome { success: true, message_id: Some(email.message_id.clone()), error_kind: None, retryable: false }
        }
    }
    let sender = Arc::new(CapturingSender { captured: Mutex::new(None) });
    let tool = SendEmail::with_deps_and_sender(store, secret_store, sender.clone());

    tool.invoke(
        json!({
            "to": "user@example.com",
            "subject": "Original subject",
            "body": "body",
            "in_reply_to_message_id": "abc123@example.com",
        }),
        &make_ctx(),
    )
    .await
    .unwrap();

    let captured = sender.captured.lock().await.clone().unwrap();
    assert_eq!(captured.subject, "Re: Original subject");
    assert_eq!(captured.in_reply_to.as_deref(), Some("<abc123@example.com>"));
    assert_eq!(captured.references.as_deref(), Some("<abc123@example.com>"));
}
