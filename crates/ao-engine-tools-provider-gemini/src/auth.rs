//! Header construction and endpoint URL builder for the Gemini Generative Language API.
//!
//! Vertex AI bearer-token auth is not implemented yet; the `apply_auth` surface
//! is designed to accommodate it without a breaking change.

use ao_engine_tools_provider_config::GeminiConfig;

/// Build the streaming `generateContent` endpoint URL from config.
///
/// The model name is embedded in the path per Gemini's URL scheme.
/// `base_url` defaults to `https://generativelanguage.googleapis.com/v1beta`,
/// but can be overridden for proxies or future Vertex endpoints.
pub fn endpoint_url(config: &GeminiConfig) -> String {
    format!(
        "{}/models/{}:streamGenerateContent?alt=sse",
        config.base_url, config.model
    )
}

/// Attach auth and content-type headers to a request builder.
///
/// Named `apply_auth` (not `apply_api_key`) so a future Vertex bearer-token
/// path can slot in without changing callsites.
pub fn apply_auth(
    builder: reqwest::RequestBuilder,
    config: &GeminiConfig,
) -> reqwest::RequestBuilder {
    builder
        .header("x-goog-api-key", &config.api_key)
        .header("Content-Type", "application/json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_default_url() -> GeminiConfig {
        GeminiConfig {
            api_key: "AIza-TEST-KEY".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            model: "gemini-1.5-pro".into(),
        }
    }

    fn config_custom_url() -> GeminiConfig {
        GeminiConfig {
            api_key: "AIza-TEST-KEY".into(),
            base_url: "http://localhost:9090/v1beta".into(),
            model: "gemini-1.5-pro".into(),
        }
    }

    #[test]
    fn endpoint_url_uses_default_base_url() {
        let config = config_default_url();
        assert_eq!(
            endpoint_url(&config),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn endpoint_url_uses_override_base_url() {
        let config = config_custom_url();
        assert_eq!(
            endpoint_url(&config),
            "http://localhost:9090/v1beta/models/gemini-1.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn apply_auth_sets_api_key_and_content_type_headers() {
        let client = reqwest::Client::new();
        let config = config_default_url();
        let request = apply_auth(client.post(endpoint_url(&config)), &config)
            .build()
            .expect("request build should succeed");

        let headers = request.headers();
        assert_eq!(
            headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()),
            Some("AIza-TEST-KEY"),
        );
        assert_eq!(
            headers.get("Content-Type").and_then(|v| v.to_str().ok()),
            Some("application/json"),
        );
    }
}
