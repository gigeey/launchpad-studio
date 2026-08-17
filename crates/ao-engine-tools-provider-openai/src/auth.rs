//! Header construction and endpoint URL builder for the OpenAI Chat Completions API.

use ao_engine_tools_provider_config::OpenAIConfig;

/// Build the Chat Completions endpoint URL from config.
///
/// The `base_url` already includes the `/v1` path prefix (default:
/// `https://api.openai.com/v1`), so this function only appends
/// `/chat/completions`.  This lets users point at OpenAI-compatible proxies
/// (Azure OpenAI, vLLM, OpenRouter) by overriding `base_url` without also
/// having to re-specify the path suffix.
pub fn endpoint_url(config: &OpenAIConfig) -> String {
    format!("{}/chat/completions", config.base_url)
}

/// Attach the required OpenAI headers to a request builder.
///
/// Always sets:
/// - `Authorization: Bearer <api_key>`
/// - `Content-Type: application/json`
///
/// Conditionally sets (when `Some`):
/// - `OpenAI-Organization: <org_id>`
/// - `OpenAI-Project: <project_id>`
///
/// Auth failures (401/403) are handled by the response layer as
/// `ProviderError::Transport("{status}: {body}")`.
pub fn apply_headers(
    builder: reqwest::RequestBuilder,
    config: &OpenAIConfig,
) -> reqwest::RequestBuilder {
    let builder = builder
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("Content-Type", "application/json");

    let builder = if let Some(org) = &config.organization {
        builder.header("OpenAI-Organization", org)
    } else {
        builder
    };

    if let Some(project) = &config.project {
        builder.header("OpenAI-Project", project)
    } else {
        builder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_full() -> OpenAIConfig {
        OpenAIConfig {
            api_key: "sk-TEST-KEY".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            organization: Some("org-TESTORG".into()),
            project: Some("proj-TESTPROJ".into()),
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
}
    }

    fn config_minimal() -> OpenAIConfig {
        OpenAIConfig {
            api_key: "sk-TEST-KEY".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            organization: None,
            project: None,
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
}
    }

    #[test]
    fn all_four_headers_set_when_org_and_project_present() {
        let client = reqwest::Client::new();
        let builder = client.post(endpoint_url(&config_full()));
        let request = apply_headers(builder, &config_full())
            .build()
            .expect("request build should succeed");

        let headers = request.headers();
        assert_eq!(
            headers.get("Authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer sk-TEST-KEY"),
        );
        assert_eq!(
            headers.get("Content-Type").and_then(|v| v.to_str().ok()),
            Some("application/json"),
        );
        assert_eq!(
            headers
                .get("OpenAI-Organization")
                .and_then(|v| v.to_str().ok()),
            Some("org-TESTORG"),
        );
        assert_eq!(
            headers
                .get("OpenAI-Project")
                .and_then(|v| v.to_str().ok()),
            Some("proj-TESTPROJ"),
        );
    }

    #[test]
    fn only_auth_and_content_type_when_org_and_project_absent() {
        let client = reqwest::Client::new();
        let config = config_minimal();
        let builder = client.post(endpoint_url(&config));
        let request = apply_headers(builder, &config)
            .build()
            .expect("request build should succeed");

        let headers = request.headers();
        assert_eq!(
            headers.get("Authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer sk-TEST-KEY"),
        );
        assert_eq!(
            headers.get("Content-Type").and_then(|v| v.to_str().ok()),
            Some("application/json"),
        );
        assert!(
            headers.get("OpenAI-Organization").is_none(),
            "OpenAI-Organization header must be absent"
        );
        assert!(
            headers.get("OpenAI-Project").is_none(),
            "OpenAI-Project header must be absent"
        );
    }

    #[test]
    fn endpoint_url_composed_correctly_for_proxy_base_url() {
        let config = OpenAIConfig {
            api_key: "sk-TEST".into(),
            base_url: "http://localhost:8080/v1".into(),
            model: "gpt-4o".into(),
            organization: None,
            project: None,
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
};
        assert_eq!(
            endpoint_url(&config),
            "http://localhost:8080/v1/chat/completions"
        );
    }
}
