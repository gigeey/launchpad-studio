use std::fs;
use std::path::Path;

use ao_protocol::data_root::DATA_DIR_ENV_VAR;

use crate::test_env::{lock_env, EnvGuard};
use crate::{absorb_plaintext_api_keys, api_key_fingerprint, OpenAIConfig, ProviderConfig, ProviderConfigError, SecretVault};

const FIXTURE_TOML: &str = r#"
[anthropic]
api_key = "sk-ant-test"
base_url = "https://api.anthropic.com"
model = "claude-opus-4-7"
"#;

const OPENAI_FULL_FIXTURE: &str = r#"
[openai]
api_key = "sk-openai-test"
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
organization = "org-abc123"
project = "proj-xyz789"
"#;

const OPENAI_MINIMAL_FIXTURE: &str = r#"
[openai]
api_key = "sk-openai-minimal"
"#;

const OPENROUTER_FULL_FIXTURE: &str = r#"
[openrouter]
api_key = "sk-or-test"
base_url = "https://openrouter.ai/api/v1"
model = "anthropic/claude-opus-4.7"
"#;

const OPENROUTER_MINIMAL_FIXTURE: &str = r#"
[openrouter]
api_key = "sk-or-minimal"
"#;

const BOTH_PROVIDERS_FIXTURE: &str = r#"
[anthropic]
api_key = "sk-ant-test"
base_url = "https://api.anthropic.com"
model = "claude-opus-4-7"

[openai]
api_key = "sk-openai-test"
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
"#;

/// Forces the file-backed [`SecretVault`] fallback so `ProviderConfig`'s
/// vault-touching methods can't reach the real OS keychain during a test
/// run, and points the data root at a fresh tempdir. Must be called while
/// holding [`lock_env`]'s guard.
const FORCE_FILE_VAULT_ENV_VAR: &str = "LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK";

fn set_up(dir: &Path) -> (EnvGuard, EnvGuard) {
    let dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.to_str().unwrap());
    let fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");
    (dd, fb)
}

#[test]
fn round_trip_fixture_toml() {
    let cfg: ProviderConfig = toml::from_str(FIXTURE_TOML).expect("parse fixture");
    let ant = cfg.anthropic.as_ref().expect("anthropic section present");
    assert_eq!(ant.api_key, "sk-ant-test");
    assert_eq!(ant.base_url, "https://api.anthropic.com");
    assert_eq!(ant.model, "claude-opus-4-7");

    let serialized = toml::to_string(&cfg).expect("serialize");
    let round_tripped: ProviderConfig = toml::from_str(&serialized).expect("re-parse");
    let ant2 = round_tripped.anthropic.as_ref().expect("anthropic present after round-trip");
    assert_eq!(ant2.api_key, ant.api_key);
    assert_eq!(ant2.base_url, ant.base_url);
    assert_eq!(ant2.model, ant.model);
}

#[test]
fn load_reads_from_data_dir_env_var() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(&path, FIXTURE_TOML).expect("write fixture");
    let (_dd, _fb) = set_up(dir.path());

    let cfg = ProviderConfig::load().expect("load succeeds");
    let ant = cfg.anthropic.expect("anthropic present");
    assert_eq!(ant.api_key, "sk-ant-test");
}

#[test]
fn not_found_on_missing_file() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    let err = ProviderConfig::load().expect_err("should fail on missing file");
    assert!(matches!(err, ProviderConfigError::NotFound { .. }));
}

#[test]
fn parse_error_on_malformed_toml() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(&path, "[[not valid toml {{{").expect("write bad fixture");
    let (_dd, _fb) = set_up(dir.path());

    let err = ProviderConfig::load().expect_err("should fail on malformed TOML");
    assert!(
        matches!(err, ProviderConfigError::Parse(_)),
        "expected Parse, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("line") || msg.contains("column") || msg.contains("TOML"),
        "error message should include location context: {msg}"
    );
}

#[test]
fn config_path_honours_env_var() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());

    let p = ProviderConfig::config_path().expect("config_path");
    assert_eq!(p.parent().unwrap(), dir.path());
    assert_eq!(p.file_name().unwrap(), "providers.toml");
}

#[test]
fn anthropic_config_serde_defaults() {
    let minimal = r#"
[anthropic]
api_key = "sk-ant-minimal"
"#;
    let cfg: ProviderConfig = toml::from_str(minimal).expect("parse minimal");
    let ant = cfg.anthropic.expect("anthropic present");
    assert_eq!(ant.base_url, "https://api.anthropic.com");
    assert_eq!(ant.model, "claude-opus-4-7");
}

#[test]
fn anthropic_config_missing_api_key_defaults_to_empty() {
    let no_key = r#"
[anthropic]
base_url = "https://api.anthropic.com"
"#;
    let cfg: ProviderConfig = toml::from_str(no_key).expect("parse section without api_key");
    let ant = cfg.anthropic.expect("anthropic present");
    assert_eq!(ant.api_key, "", "a scrubbed section must still deserialize, with an empty api_key");
}

#[test]
fn openai_config_round_trip_all_fields() {
    let cfg: ProviderConfig = toml::from_str(OPENAI_FULL_FIXTURE).expect("parse openai full");
    let oai = cfg.openai.as_ref().expect("openai section present");
    assert_eq!(oai.api_key, "sk-openai-test");
    assert_eq!(oai.base_url, "https://api.openai.com/v1");
    assert_eq!(oai.model, "gpt-4o");
    assert_eq!(oai.organization.as_deref(), Some("org-abc123"));
    assert_eq!(oai.project.as_deref(), Some("proj-xyz789"));

    let serialized = toml::to_string(&cfg).expect("serialize");
    let round_tripped: ProviderConfig = toml::from_str(&serialized).expect("re-parse");
    let oai2 = round_tripped.openai.as_ref().expect("openai present after round-trip");
    assert_eq!(oai2.api_key, oai.api_key);
    assert_eq!(oai2.base_url, oai.base_url);
    assert_eq!(oai2.model, oai.model);
    assert_eq!(oai2.organization, oai.organization);
    assert_eq!(oai2.project, oai.project);
}

#[test]
fn openai_config_serde_defaults() {
    let cfg: ProviderConfig = toml::from_str(OPENAI_MINIMAL_FIXTURE).expect("parse openai minimal");
    let oai = cfg.openai.expect("openai present");
    assert_eq!(oai.api_key, "sk-openai-minimal");
    assert_eq!(oai.base_url, "https://api.openai.com/v1");
    assert_eq!(oai.model, "gpt-4o");
    assert!(oai.organization.is_none());
    assert!(oai.project.is_none());
}

#[test]
fn load_reads_openai_config_from_data_dir() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(&path, OPENAI_FULL_FIXTURE).expect("write fixture");
    let (_dd, _fb) = set_up(dir.path());

    let cfg = ProviderConfig::load().expect("load succeeds");
    let oai = cfg.openai.expect("openai present");
    assert_eq!(oai.api_key, "sk-openai-test");
    assert_eq!(oai.organization.as_deref(), Some("org-abc123"));
}

#[test]
fn openrouter_config_round_trip_all_fields() {
    let cfg: ProviderConfig = toml::from_str(OPENROUTER_FULL_FIXTURE).expect("parse openrouter full");
    let router = cfg.openrouter.as_ref().expect("openrouter section present");
    assert_eq!(router.api_key, "sk-or-test");
    assert_eq!(router.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(router.model, "anthropic/claude-opus-4.7");

    let serialized = toml::to_string(&cfg).expect("serialize");
    let round_tripped: ProviderConfig = toml::from_str(&serialized).expect("re-parse");
    let router2 = round_tripped.openrouter.as_ref().expect("openrouter present after round-trip");
    assert_eq!(router2.api_key, router.api_key);
    assert_eq!(router2.base_url, router.base_url);
    assert_eq!(router2.model, router.model);
}

#[test]
fn openrouter_config_serde_defaults() {
    let cfg: ProviderConfig = toml::from_str(OPENROUTER_MINIMAL_FIXTURE).expect("parse openrouter minimal");
    let router = cfg.openrouter.expect("openrouter present");
    assert_eq!(router.api_key, "sk-or-minimal");
    assert_eq!(router.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(router.model, "openrouter/auto");
}

#[test]
fn openrouter_config_defaults_differ_from_openai_defaults() {
    // The whole point of a dedicated `OpenRouterConfig` type instead of
    // reusing `OpenAIConfig` is that each gets its own serde defaults —
    // this pins that behavior down directly.
    let cfg: ProviderConfig = toml::from_str("[openrouter]\n").expect("parse bare section");
    let router = cfg.openrouter.expect("openrouter present");
    assert_eq!(router.base_url, "https://openrouter.ai/api/v1");
    assert_ne!(router.base_url, "https://api.openai.com/v1");
    assert_eq!(router.model, "openrouter/auto");
    assert_ne!(router.model, "gpt-4o");
}

#[test]
fn load_reads_openrouter_config_from_data_dir() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(&path, OPENROUTER_FULL_FIXTURE).expect("write fixture");
    let (_dd, _fb) = set_up(dir.path());

    let cfg = ProviderConfig::load().expect("load succeeds");
    let router = cfg.openrouter.expect("openrouter present");
    assert_eq!(router.api_key, "sk-or-test");
}

#[test]
fn openrouter_debug_redacts_api_key() {
    let cfg: ProviderConfig = toml::from_str(OPENROUTER_FULL_FIXTURE).expect("parse fixture");
    let dbg = format!("{:?}", cfg.openrouter.expect("openrouter present"));
    assert!(!dbg.contains("sk-or-test"), "api_key leaked into Debug output: {dbg}");
    assert!(dbg.contains("REDACTED"));
}

#[test]
fn openrouter_config_adapts_onto_openai_config_shape() {
    let cfg: ProviderConfig = toml::from_str(OPENROUTER_FULL_FIXTURE).expect("parse fixture");
    let router = cfg.openrouter.expect("openrouter present");
    let adapted: crate::OpenAIConfig = router.into();
    assert_eq!(adapted.api_key, "sk-or-test");
    assert_eq!(adapted.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(adapted.model, "anthropic/claude-opus-4.7");
    assert_eq!(adapted.organization, None);
    assert_eq!(adapted.project, None);
}

#[test]
fn save_provider_accepts_openrouter() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider("openrouter", "sk-or-new", Some("https://openrouter.ai/api/v1"), None, None, None, None)
        .expect("save");

    let cfg = ProviderConfig::load().expect("load after save");
    let router = cfg.openrouter.expect("openrouter present");
    assert_eq!(router.api_key, "sk-or-new");
    assert_eq!(router.base_url, "https://openrouter.ai/api/v1");
}

#[test]
fn delete_provider_removes_openrouter_from_vault() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider("openrouter", "sk-or-to-delete", None, None, None, None, None).expect("save");
    let vault = SecretVault::open().expect("open vault");
    assert!(vault.get_provider("openrouter").expect("get before delete").is_some());

    ProviderConfig::delete_provider("openrouter").expect("delete");
    assert!(vault.get_provider("openrouter").expect("get after delete").is_none());
}

#[test]
fn both_providers_coexist() {
    let cfg: ProviderConfig = toml::from_str(BOTH_PROVIDERS_FIXTURE).expect("parse both");
    let ant = cfg.anthropic.as_ref().expect("anthropic present");
    let oai = cfg.openai.as_ref().expect("openai present");
    assert_eq!(ant.api_key, "sk-ant-test");
    assert_eq!(oai.api_key, "sk-openai-test");
}

// --- Debug redaction ---

#[test]
fn anthropic_debug_redacts_api_key() {
    let cfg: ProviderConfig = toml::from_str(FIXTURE_TOML).expect("parse fixture");
    let dbg = format!("{:?}", cfg.anthropic.expect("anthropic present"));
    assert!(!dbg.contains("sk-ant-test"), "api_key leaked into Debug output: {dbg}");
    assert!(dbg.contains("REDACTED"));
}

#[test]
fn openai_debug_redacts_api_key() {
    let cfg: ProviderConfig = toml::from_str(OPENAI_FULL_FIXTURE).expect("parse fixture");
    let dbg = format!("{:?}", cfg.openai.expect("openai present"));
    assert!(!dbg.contains("sk-openai-test"), "api_key leaked into Debug output: {dbg}");
    assert!(dbg.contains("REDACTED"));
    assert!(dbg.contains("org-abc123"), "non-secret field should still print");
}

// --- statuses() ---

#[test]
fn statuses_missing_file_returns_all_false() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    let statuses = ProviderConfig::statuses().expect("statuses should not error on missing file");
    assert_eq!(statuses.len(), 4);
    assert!(statuses.iter().all(|s| !s.has_api_key));
    assert!(statuses.iter().all(|s| s.base_url.is_none()));
}

#[test]
fn statuses_never_include_the_api_key() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("providers.toml"), BOTH_PROVIDERS_FIXTURE).expect("write fixture");
    let (_dd, _fb) = set_up(dir.path());

    let statuses = ProviderConfig::statuses().expect("statuses");
    let ant = statuses.iter().find(|s| s.provider == "anthropic").expect("anthropic status");
    assert!(ant.has_api_key);
    assert_eq!(ant.base_url.as_deref(), Some("https://api.anthropic.com"));
    let oai = statuses.iter().find(|s| s.provider == "openai").expect("openai status");
    assert!(oai.has_api_key);
    let gem = statuses.iter().find(|s| s.provider == "gemini").expect("gemini status");
    assert!(!gem.has_api_key);

    // Serialize as the HTTP route would and assert the secret never appears.
    let json = serde_json::to_string(&statuses).expect("serialize");
    assert!(!json.contains("sk-ant-test"));
    assert!(!json.contains("sk-openai-test"));
    assert!(!json.contains("\"api_key\""), "response must not carry a literal api_key field: {json}");
}

#[test]
fn statuses_reports_has_api_key_from_vault_when_toml_lacks_it() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("providers.toml"),
        "[gemini]\nbase_url = \"https://generativelanguage.googleapis.com/v1beta\"\n",
    )
    .expect("write fixture without api_key");
    let (_dd, _fb) = set_up(dir.path());

    let vault = SecretVault::open().expect("open vault");
    vault.set_provider("gemini", "sk-gemini-from-vault").expect("seed vault directly");

    let statuses = ProviderConfig::statuses().expect("statuses");
    let gem = statuses.iter().find(|s| s.provider == "gemini").expect("gemini status");
    assert!(gem.has_api_key, "has_api_key must be sourced from the vault, not the toml struct");
}

// --- save_provider() / delete_provider() ---

#[test]
fn save_provider_creates_file_and_sets_key() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider("anthropic", "sk-ant-new", None, None, None, None, None).expect("save");

    let cfg = ProviderConfig::load().expect("load after save");
    let ant = cfg.anthropic.expect("anthropic present");
    assert_eq!(ant.api_key, "sk-ant-new");
}

#[test]
fn save_provider_rejects_unknown_name() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    let err = ProviderConfig::save_provider("not-a-provider", "sk-x", None, None, None, None, None).expect_err("should reject");
    assert!(matches!(err, ProviderConfigError::UnknownProvider(_)));
}

#[test]
fn save_provider_preserves_other_sections_and_comments() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(
        &path,
        "# hand-written note\n[openai]\napi_key = \"sk-openai-test\"\nmodel = \"gpt-4o\"\n",
    )
    .expect("write fixture");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider("anthropic", "sk-ant-new", None, None, None, None, None).expect("save");

    let raw = fs::read_to_string(&path).expect("read back");
    assert!(raw.contains("# hand-written note"), "hand-written comment was dropped: {raw}");

    let cfg = ProviderConfig::load().expect("load after save");
    assert_eq!(cfg.openai.expect("openai preserved").api_key, "sk-openai-test");
    assert_eq!(cfg.anthropic.expect("anthropic added").api_key, "sk-ant-new");
}

#[test]
fn save_provider_overwrites_existing_key() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(&path, FIXTURE_TOML).expect("write fixture");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider("anthropic", "sk-ant-rotated", None, None, None, None, None).expect("save");

    let raw = fs::read_to_string(&path).expect("read back");
    assert!(
        !raw.contains("api_key"),
        "save_provider must strip a pre-existing plaintext api_key from the file too: {raw}"
    );

    let cfg = ProviderConfig::load().expect("load");
    assert_eq!(cfg.anthropic.expect("anthropic present").api_key, "sk-ant-rotated");
}

#[cfg(unix)]
#[test]
fn save_provider_sets_restrictive_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider("anthropic", "sk-ant-new", None, None, None, None, None).expect("save");

    let path = ProviderConfig::config_path().expect("config_path");
    let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "providers.toml should be owner-read/write only, got {mode:o}");
}

#[test]
fn save_provider_never_writes_api_key_to_toml() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider("anthropic", "sk-ant-direct-to-vault", Some("https://api.anthropic.com"), None, None, None, None)
        .expect("save");

    let path = ProviderConfig::config_path().expect("config_path");
    let raw = fs::read_to_string(&path).expect("read back");
    assert!(
        !raw.contains("api_key"),
        "save_provider must never write api_key to providers.toml, even transiently: {raw}"
    );

    let vault = SecretVault::open().expect("open vault");
    assert_eq!(
        vault.get_provider("anthropic").expect("get"),
        Some("sk-ant-direct-to-vault".to_owned())
    );
}

// --- tuning knobs: max_output_tokens / max_context_tokens / reasoning_effort ---

#[test]
fn anthropic_config_parses_tuning_knobs_when_present() {
    let toml_str = r#"
[anthropic]
api_key = "sk-ant-test"
max_output_tokens = 4096
max_context_tokens = 100000
reasoning_effort = "medium"
"#;
    let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
    let anthropic = cfg.anthropic.expect("anthropic section present");
    assert_eq!(anthropic.max_output_tokens, Some(4096));
    assert_eq!(anthropic.max_context_tokens, Some(100_000));
    assert_eq!(anthropic.reasoning_effort, Some(ao_protocol::agent::ReasoningEffort::Medium));
}

#[test]
fn anthropic_config_tuning_knobs_default_to_none_when_absent() {
    let cfg: ProviderConfig = toml::from_str(FIXTURE_TOML).expect("parse");
    let anthropic = cfg.anthropic.expect("anthropic section present");
    assert!(anthropic.max_output_tokens.is_none());
    assert!(anthropic.max_context_tokens.is_none());
    assert!(anthropic.reasoning_effort.is_none());
}

#[test]
fn openai_config_parses_tuning_knobs_when_present() {
    let toml_str = r#"
[openai]
api_key = "sk-openai-test"
max_output_tokens = 8192
max_context_tokens = 50000
reasoning_effort = "high"
"#;
    let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
    let openai = cfg.openai.expect("openai section present");
    assert_eq!(openai.max_output_tokens, Some(8192));
    assert_eq!(openai.max_context_tokens, Some(50_000));
    assert_eq!(openai.reasoning_effort, Some(ao_protocol::agent::ReasoningEffort::High));
}

#[test]
fn openrouter_config_parses_tuning_knobs_when_present() {
    let toml_str = r#"
[openrouter]
api_key = "sk-or-test"
max_output_tokens = 2048
reasoning_effort = "low"
"#;
    let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
    let openrouter = cfg.openrouter.expect("openrouter section present");
    assert_eq!(openrouter.max_output_tokens, Some(2048));
    assert!(openrouter.max_context_tokens.is_none());
    assert_eq!(openrouter.reasoning_effort, Some(ao_protocol::agent::ReasoningEffort::Low));

    // Adapting onto OpenAIConfig's shape (the shared OpenAI-compatible
    // transport) must carry the knobs through, same as base_url/model.
    let adapted: OpenAIConfig = openrouter.into();
    assert_eq!(adapted.max_output_tokens, Some(2048));
    assert_eq!(adapted.reasoning_effort, Some(ao_protocol::agent::ReasoningEffort::Low));
}

#[test]
fn save_provider_writes_tuning_knobs_to_toml() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider(
        "anthropic",
        "sk-ant-tuning-test",
        None,
        None,
        Some(4096),
        Some(80_000),
        Some(ao_protocol::agent::ReasoningEffort::Medium),
    )
    .expect("save");

    let cfg = ProviderConfig::load().expect("load");
    let anthropic = cfg.anthropic.expect("anthropic present");
    assert_eq!(anthropic.max_output_tokens, Some(4096));
    assert_eq!(anthropic.max_context_tokens, Some(80_000));
    assert_eq!(anthropic.reasoning_effort, Some(ao_protocol::agent::ReasoningEffort::Medium));
}

#[test]
fn save_provider_omitted_tuning_knobs_leave_existing_values_untouched() {
    // Same "omitted means leave whatever's already stored" merge semantics
    // `base_url`/`model` already have.
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider(
        "anthropic",
        "sk-ant-first",
        None,
        None,
        Some(4096),
        None,
        Some(ao_protocol::agent::ReasoningEffort::Low),
    )
    .expect("first save");

    // Second save omits all three tuning knobs — they must survive untouched.
    ProviderConfig::save_provider("anthropic", "sk-ant-second", None, None, None, None, None)
        .expect("second save");

    let cfg = ProviderConfig::load().expect("load");
    let anthropic = cfg.anthropic.expect("anthropic present");
    assert_eq!(anthropic.max_output_tokens, Some(4096));
    assert_eq!(anthropic.reasoning_effort, Some(ao_protocol::agent::ReasoningEffort::Low));
}

#[test]
fn statuses_reports_tuning_knobs() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider(
        "openai",
        "sk-openai-status-test",
        None,
        None,
        Some(1024),
        Some(20_000),
        Some(ao_protocol::agent::ReasoningEffort::High),
    )
    .expect("save");

    let statuses = ProviderConfig::statuses().expect("statuses");
    let openai_status = statuses.iter().find(|s| s.provider == "openai").expect("openai status present");
    assert_eq!(openai_status.max_output_tokens, Some(1024));
    assert_eq!(openai_status.max_context_tokens, Some(20_000));
    assert_eq!(openai_status.reasoning_effort, Some(ao_protocol::agent::ReasoningEffort::High));
}

#[test]
fn delete_provider_removes_section() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("providers.toml"), BOTH_PROVIDERS_FIXTURE).expect("write fixture");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::delete_provider("anthropic").expect("delete");

    let cfg = ProviderConfig::load().expect("load after delete");
    assert!(cfg.anthropic.is_none());
    assert!(cfg.openai.expect("openai untouched").api_key == "sk-openai-test");
}

#[test]
fn delete_provider_missing_file_is_noop() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::delete_provider("openai").expect("delete on missing file must not error");
}

#[test]
fn delete_provider_removes_from_vault() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_dd, _fb) = set_up(dir.path());

    ProviderConfig::save_provider("openai", "sk-openai-to-delete", None, None, None, None, None).expect("save");
    let vault = SecretVault::open().expect("open vault");
    assert!(vault.get_provider("openai").expect("get before delete").is_some());

    ProviderConfig::delete_provider("openai").expect("delete");
    assert!(vault.get_provider("openai").expect("get after delete").is_none());
}

// --- Migration + scrub (crates::absorb_plaintext_api_keys) ---
//
// `absorb_plaintext_api_keys` takes the scrub decision as an explicit `bool`
// rather than deriving it from a vault's actual backend, so these tests can
// exercise both branches deterministically with a file-fallback vault —
// never the real OS keychain — while still testing the exact function
// `ProviderConfig::load` calls with `vault.is_keychain_backed()` in
// production.

#[test]
fn absorb_plaintext_api_keys_scrubs_when_scrub_true() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vault = SecretVault::new_with_file_fallback(dir.path().to_path_buf());

    let mut doc = "[anthropic]\napi_key = \"sk-ant-plain\"\nbase_url = \"https://api.anthropic.com\"\n"
        .parse::<toml_edit::DocumentMut>()
        .expect("parse");

    let changed = absorb_plaintext_api_keys(&mut doc, &vault, true).expect("absorb");
    assert!(changed, "doc must be reported changed when a key is scrubbed");

    assert_eq!(vault.get_provider("anthropic").expect("get"), Some("sk-ant-plain".to_owned()));
    assert!(
        doc["anthropic"].as_table().expect("table").get("api_key").is_none(),
        "api_key must be scrubbed from the document when scrub=true (keychain-backed vault)"
    );
    assert_eq!(
        doc["anthropic"]["base_url"].as_str(),
        Some("https://api.anthropic.com"),
        "non-secret fields must survive the scrub"
    );
}

#[test]
fn absorb_plaintext_api_keys_leaves_key_when_scrub_false() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vault = SecretVault::new_with_file_fallback(dir.path().to_path_buf());

    let mut doc = "[openai]\napi_key = \"sk-openai-plain\"\n"
        .parse::<toml_edit::DocumentMut>()
        .expect("parse");

    let changed = absorb_plaintext_api_keys(&mut doc, &vault, false).expect("absorb");
    assert!(!changed, "doc must not be reported changed when scrub=false (file-vault backend)");

    assert_eq!(vault.get_provider("openai").expect("get"), Some("sk-openai-plain".to_owned()));
    assert_eq!(
        doc["openai"]["api_key"].as_str(),
        Some("sk-openai-plain"),
        "file-vault mode must leave the plaintext key in place as a valid injection channel"
    );
}

#[test]
fn absorb_plaintext_api_keys_does_not_clobber_existing_vault_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vault = SecretVault::new_with_file_fallback(dir.path().to_path_buf());
    vault.set_provider("anthropic", "sk-ant-fresh-from-vault").expect("seed vault");

    let mut doc = "[anthropic]\napi_key = \"sk-ant-stale-from-file\"\n"
        .parse::<toml_edit::DocumentMut>()
        .expect("parse");

    absorb_plaintext_api_keys(&mut doc, &vault, true).expect("absorb");

    assert_eq!(
        vault.get_provider("anthropic").expect("get"),
        Some("sk-ant-fresh-from-vault".to_owned()),
        "a stale plaintext value in the file must not overwrite a newer vault entry"
    );
}

#[test]
fn absorb_plaintext_api_keys_ignores_missing_or_empty_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vault = SecretVault::new_with_file_fallback(dir.path().to_path_buf());

    let mut doc = "[anthropic]\nbase_url = \"https://api.anthropic.com\"\n\n[openai]\napi_key = \"\"\n"
        .parse::<toml_edit::DocumentMut>()
        .expect("parse");

    let changed = absorb_plaintext_api_keys(&mut doc, &vault, true).expect("absorb");
    assert!(!changed);
    assert!(vault.get_provider("anthropic").expect("get").is_none());
    assert!(vault.get_provider("openai").expect("get").is_none());
}

// --- load(): migration + scrub via the public API (file-vault backend) ---

#[test]
fn load_absorbs_into_vault_but_leaves_file_intact_on_file_vault_backend() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(&path, FIXTURE_TOML).expect("write fixture");
    let (_dd, _fb) = set_up(dir.path());

    let cfg = ProviderConfig::load().expect("load");
    assert_eq!(cfg.anthropic.expect("anthropic present").api_key, "sk-ant-test");

    let vault = SecretVault::open().expect("open vault");
    assert_eq!(vault.get_provider("anthropic").expect("get"), Some("sk-ant-test".to_owned()));

    let raw = fs::read_to_string(&path).expect("read back");
    assert!(
        raw.contains("api_key"),
        "file-vault backend must leave the plaintext key in providers.toml as a valid injection channel: {raw}"
    );
}

#[test]
fn load_populates_api_key_from_vault_when_toml_lacks_it() {
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(
        &path,
        "[anthropic]\nbase_url = \"https://api.anthropic.com\"\nmodel = \"claude-opus-4-7\"\n",
    )
    .expect("write fixture without api_key");
    let (_dd, _fb) = set_up(dir.path());

    let vault = SecretVault::open().expect("open vault");
    vault.set_provider("anthropic", "sk-ant-from-vault").expect("seed vault directly");

    let cfg = ProviderConfig::load().expect("load");
    let ant = cfg.anthropic.expect("anthropic present");
    assert_eq!(ant.api_key, "sk-ant-from-vault");
    assert_eq!(ant.base_url, "https://api.anthropic.com");
    assert_eq!(ant.model, "claude-opus-4-7");
}

// --- Real-keychain-gated coverage (skipped unless opted in) ---
//
// `save_provider` never branches on the vault backend when deciding whether
// to write `api_key` to the file — it always strips it — so the file-vault
// coverage above already exercises the same code path a keychain backend
// would run. This test adds direct confirmation against the real keychain
// for defense in depth, matching the pattern used by this crate's other
// vault-backed stores.

#[test]
#[ignore = "requires OS keychain; run with LAUNCHPAD_TEST_KEYCHAIN=1 cargo test -- --ignored"]
fn save_provider_never_writes_api_key_to_toml_keychain_backend() {
    if std::env::var("LAUNCHPAD_TEST_KEYCHAIN").is_err() {
        return;
    }
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    // Deliberately no FORCE_FILE_VAULT_ENV_VAR — exercises the real keychain backend.

    let _ = ProviderConfig::delete_provider("anthropic"); // clean up any leftover from a prior run

    ProviderConfig::save_provider("anthropic", "sk-ant-keychain-mode-test", None, None, None, None, None).expect("save");

    let path = ProviderConfig::config_path().expect("config_path");
    let raw = fs::read_to_string(&path).expect("read back");
    assert!(!raw.contains("api_key"), "keychain-backend save must never write api_key to providers.toml: {raw}");

    let vault = SecretVault::open().expect("open vault");
    assert_eq!(
        vault.get_provider("anthropic").expect("get"),
        Some("sk-ant-keychain-mode-test".to_owned())
    );

    ProviderConfig::delete_provider("anthropic").expect("cleanup");
}

#[test]
#[ignore = "requires OS keychain; run with LAUNCHPAD_TEST_KEYCHAIN=1 cargo test -- --ignored"]
fn load_scrubs_plaintext_key_on_keychain_backend() {
    if std::env::var("LAUNCHPAD_TEST_KEYCHAIN").is_err() {
        return;
    }
    let _lock = lock_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    // Deliberately no FORCE_FILE_VAULT_ENV_VAR — exercises the real keychain backend.

    let _ = ProviderConfig::delete_provider("anthropic"); // clean up any leftover from a prior run
    fs::write(&path, FIXTURE_TOML).expect("write fixture");

    let cfg = ProviderConfig::load().expect("load");
    assert_eq!(cfg.anthropic.expect("anthropic present").api_key, "sk-ant-test");

    let raw = fs::read_to_string(&path).expect("read back");
    assert!(!raw.contains("api_key"), "keychain backend must scrub the plaintext key from the file: {raw}");

    ProviderConfig::delete_provider("anthropic").expect("cleanup");
}

// ---------------------------------------------------------------------------
// providers.toml.example
// ---------------------------------------------------------------------------
//
// The shipped example file is the first thing a new user edits, so it is
// checked against the real deserializer rather than proofread. These tests
// cover the file's TOML shape only — the vault behaviour it describes is
// covered by the `load()` tests above.

/// The example file exactly as it ships, so a drift in the repo copy fails
/// the build rather than the reader's first attempt.
const EXAMPLE_TOML: &str = include_str!("../../../providers.toml.example");

/// Uncomments the example the way the file tells a reader to: strip one
/// leading `#` from any line that is not prose. Prose lines are written with
/// a hash and a space (`# like this`); template lines are written with a bare
/// hash (`#like_this = "..."`). The distinction is documented in the file
/// itself, and this function is the executable statement of it.
fn uncomment_example(src: &str) -> String {
    src.lines()
        .map(|line| match line.strip_prefix('#') {
            Some(rest) if !rest.starts_with(' ') => rest,
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn example_as_copied_configures_no_provider() {
    let cfg: ProviderConfig = toml::from_str(EXAMPLE_TOML).expect("example file must be valid TOML");
    assert!(cfg.anthropic.is_none(), "a freshly copied example must configure nothing");
    assert!(cfg.openai.is_none());
    assert!(cfg.openrouter.is_none());
    assert!(cfg.gemini.is_none());
}

#[test]
fn example_uncommented_matches_the_real_provider_schema() {
    let uncommented = uncomment_example(EXAMPLE_TOML);
    let cfg: ProviderConfig =
        toml::from_str(&uncommented).unwrap_or_else(|e| panic!("uncommented example must parse: {e}\n---\n{uncommented}"));

    let anthropic = cfg.anthropic.expect("[anthropic] block present");
    assert_eq!(anthropic.api_key, "");
    assert_eq!(anthropic.base_url, "https://api.anthropic.com");
    assert_eq!(anthropic.model, "claude-opus-4-7");

    let openai = cfg.openai.expect("[openai] block present");
    assert_eq!(openai.api_key, "");
    assert_eq!(openai.base_url, "https://api.openai.com/v1");
    assert_eq!(openai.model, "gpt-4o");
    // Deliberately absent from the block: an empty string here sends an empty
    // header rather than omitting it. See the note in the example file.
    assert_eq!(openai.organization, None);
    assert_eq!(openai.project, None);

    let openrouter = cfg.openrouter.expect("[openrouter] block present");
    assert_eq!(openrouter.api_key, "");
    assert_eq!(openrouter.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(openrouter.model, "openrouter/auto");

    let gemini = cfg.gemini.expect("[gemini] block present");
    assert_eq!(gemini.api_key, "");
    assert_eq!(gemini.base_url, "https://generativelanguage.googleapis.com/v1beta");
    assert_eq!(gemini.model, "gemini-1.5-pro");
}

/// The example documents each provider's `base_url`/`model` as "the defaults
/// the code falls back to". That claim is only true while the file agrees
/// with the `serde(default)` functions, so it is asserted rather than trusted.
#[test]
fn example_defaults_match_the_code_defaults() {
    let bare: ProviderConfig =
        toml::from_str("[anthropic]\n[openai]\n[openrouter]\n[gemini]\n").expect("bare sections parse");
    let uncommented: ProviderConfig = toml::from_str(&uncomment_example(EXAMPLE_TOML)).expect("example parses");

    let (b, u) = (bare.anthropic.expect("bare"), uncommented.anthropic.expect("example"));
    assert_eq!((b.base_url, b.model), (u.base_url, u.model));

    let (b, u) = (bare.openai.expect("bare"), uncommented.openai.expect("example"));
    assert_eq!((b.base_url, b.model), (u.base_url, u.model));

    let (b, u) = (bare.openrouter.expect("bare"), uncommented.openrouter.expect("example"));
    assert_eq!((b.base_url, b.model), (u.base_url, u.model));

    let (b, u) = (bare.gemini.expect("bare"), uncommented.gemini.expect("example"));
    assert_eq!((b.base_url, b.model), (u.base_url, u.model));
}

// --- api_key_fingerprint() ---

#[test]
fn fingerprint_below_boundary_length_is_none() {
    let key = "a".repeat(27);
    assert_eq!(api_key_fingerprint(&key), None);
}

#[test]
fn fingerprint_at_boundary_length_is_some() {
    let key = "a".repeat(28);
    assert_eq!(api_key_fingerprint(&key), Some("aaaaaaaaaaaa…aaaa".to_string()));
}

#[test]
fn fingerprint_distinguishes_api_key_from_oauth_token() {
    let api_key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
    let fp = api_key_fingerprint(api_key).expect("long enough for a fingerprint");
    assert!(fp.starts_with("sk-ant-api03"));
    assert!(fp.ends_with("wxyz"), "fingerprint should end with the key's real last 4 chars: {fp}");

    let oauth_token = "sk-ant-oat01-abcdefghijklmnopqrstuvwxyz";
    let oauth_fp = api_key_fingerprint(oauth_token).expect("long enough for a fingerprint");
    assert!(oauth_fp.starts_with("sk-ant-oat01"));
    assert_ne!(fp, oauth_fp, "an API key and an OAuth token must be visually distinguishable");
}

#[test]
fn fingerprint_trims_surrounding_whitespace_before_measuring() {
    let key = "a".repeat(28);
    let padded = format!("  {key}  ");
    assert_eq!(api_key_fingerprint(&padded), api_key_fingerprint(&key));
}

#[test]
fn fingerprint_on_multi_byte_key_does_not_panic() {
    let key = "é".repeat(30);
    let fp = api_key_fingerprint(&key).expect("30 chars clears the boundary");
    assert_eq!(fp, format!("{}…{}", "é".repeat(12), "é".repeat(4)));
}
