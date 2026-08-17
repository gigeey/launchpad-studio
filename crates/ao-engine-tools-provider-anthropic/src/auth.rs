//! Header construction for the Anthropic Messages API.
//!
//! The Anthropic version string is pinned to `2023-06-01` here.  That pin is
//! intentional: the version must be updated deliberately if the wire contract
//! changes — no automatic drift is wanted.

use ao_engine_tools_provider_config::AnthropicConfig;

/// Pinned Anthropic API version sent on every request.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Attach the three required Anthropic headers to a request builder.
///
/// - `x-api-key`: the API key from `config`
/// - `anthropic-version`: pinned to `2023-06-01` (see module doc for why)
/// - `content-type`: `application/json`
///
/// Auth failures (401 from upstream) are handled by the response layer, not here.
pub fn apply_headers(
    builder: reqwest::RequestBuilder,
    config: &AnthropicConfig,
) -> reqwest::RequestBuilder {
    builder
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_config() -> AnthropicConfig {
        AnthropicConfig {
            api_key: "sk-ant-TEST-KEY".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-opus-4-7".into(),
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
}
    }

    #[test]
    fn apply_headers_sets_all_three_headers() {
        let client = reqwest::Client::new();
        let builder = client.post("https://api.anthropic.com/v1/messages");
        let config = fixture_config();

        let request = apply_headers(builder, &config)
            .build()
            .expect("request build should succeed");

        let headers = request.headers();
        assert_eq!(
            headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-ant-TEST-KEY"),
        );
        assert_eq!(
            headers.get("anthropic-version").and_then(|v| v.to_str().ok()),
            Some("2023-06-01"),
        );
        assert_eq!(
            headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/json"),
        );
    }
}
