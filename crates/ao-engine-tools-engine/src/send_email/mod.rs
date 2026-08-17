mod prompt;
pub mod smtp_seam;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_engine_tools_provider_config::{ChannelSecretStore, EMAIL_PASSWORD_SECRET_ROLE};
use ao_persistence::profiles::AgentProfileStore;
use ao_protocol::{
    agent::{AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig},
    error::AoError,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use smtp_seam::{LettreSmtpSender, OutboundEmail, SendErrorKind, SmtpSender};

/// Agent-facing tool that sends an email through this agent's connected
/// Email channel binding — the key outbound half of the email channel (the
/// inbound half is `crate::channels::email::EmailTransport` over in
/// `ao-engine`, one layer down the dependency graph from this crate, which
/// is why this tool builds its own [`SmtpSender`]/error-kind shape rather
/// than importing `ao_engine::channels::SendResult` directly).
///
/// `store`, `secret_store`, and `sender` are constructor-injected so
/// `register_all` can install a stub (the name is present in the catalog,
/// but every call fails with a clear error) before the fully-wired instance
/// replaces it at `AppState` construction time — the same pattern
/// `crate::Delegate`/`crate::AgentAuthor` use for their own store injection.
pub struct SendEmail {
    store: Option<Arc<AgentProfileStore>>,
    secret_store: Option<Arc<ChannelSecretStore>>,
    sender: Arc<dyn SmtpSender>,
}

impl SendEmail {
    pub fn new() -> Self {
        Self { store: None, secret_store: None, sender: Arc::new(LettreSmtpSender) }
    }

    pub fn with_deps(store: Arc<AgentProfileStore>, secret_store: Arc<ChannelSecretStore>) -> Self {
        Self { store: Some(store), secret_store: Some(secret_store), sender: Arc::new(LettreSmtpSender) }
    }

    /// Test-only constructor: wires a fake [`SmtpSender`] so `invoke` can be
    /// exercised end-to-end without a live SMTP server.
    #[cfg(test)]
    fn with_deps_and_sender(
        store: Arc<AgentProfileStore>,
        secret_store: Arc<ChannelSecretStore>,
        sender: Arc<dyn SmtpSender>,
    ) -> Self {
        Self { store: Some(store), secret_store: Some(secret_store), sender }
    }
}

impl Default for SendEmail {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EngineTool for SendEmail {
    fn name(&self) -> &str {
        "SendEmail"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    /// Sends a real email over the network — an unpredictable external
    /// system, same reasoning as `Delegate::mcp_open_world_hint`.
    fn mcp_open_world_hint(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let (store, secret_store) = match (&self.store, &self.secret_store) {
            (Some(s), Some(ss)) => (s, ss),
            _ => {
                return Ok(ToolOutput::error(
                    "SendEmail requires an agent store and secret store (none configured in this context)",
                    false,
                ))
            }
        };

        let to = match read_address_list(&input, "to") {
            Ok(addrs) if !addrs.is_empty() => addrs,
            Ok(_) => return Ok(ToolOutput::error("'to' must include at least one address", true)),
            Err(msg) => return Ok(ToolOutput::error(msg, true)),
        };
        let subject = match input.get("subject").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => return Ok(ToolOutput::error("missing required field: subject", true)),
        };
        let body = match input.get("body").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("missing required field: body", true)),
        };
        let cc = match read_address_list(&input, "cc") {
            Ok(addrs) => addrs,
            Err(msg) => return Ok(ToolOutput::error(msg, true)),
        };
        let in_reply_to_message_id = input
            .get("in_reply_to_message_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let requested_binding_id = input.get("binding_id").and_then(Value::as_str);

        let profile = match store.get(&ctx.agent_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Ok(ToolOutput::error(format!("agent '{}' not found", ctx.agent_id), false))
            }
            Err(e) => return Ok(ToolOutput::error(format!("failed to load agent profile: {e}"), false)),
        };

        let binding = match resolve_email_binding(&profile, requested_binding_id) {
            Ok(b) => b,
            Err(msg) => return Ok(ToolOutput::error(msg, true)),
        };
        let ChannelKindConfig::Email { address, smtp_host, smtp_port, .. } = &binding.kind_config else {
            return Ok(ToolOutput::error(
                format!("binding '{}' is not an Email binding", binding.binding_id),
                true,
            ));
        };
        if smtp_host.trim().is_empty() {
            return Ok(ToolOutput::error(
                format!("binding '{}' has no smtp_host configured", binding.binding_id),
                true,
            ));
        }

        let password = match secret_store.get(&profile.id, &binding.binding_id, EMAIL_PASSWORD_SECRET_ROLE) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    format!("no password stored for email binding '{}'", binding.binding_id),
                    false,
                ))
            }
            Err(e) => return Ok(ToolOutput::error(format!("failed to read email password: {e}"), false)),
        };

        let subject = match in_reply_to_message_id {
            Some(_) => ensure_re_prefixed(&subject),
            None => subject,
        };
        let in_reply_to = in_reply_to_message_id.map(normalize_message_id);
        let references = in_reply_to.clone();
        let message_id = generate_message_id(address);

        let outbound = OutboundEmail {
            from: address.clone(),
            to,
            cc,
            subject,
            body,
            in_reply_to,
            references,
            message_id,
        };

        let outcome = self.sender.send(smtp_host, *smtp_port, address, &password, &outbound).await;

        if outcome.success {
            Ok(ToolOutput::structured(json!({
                "success": true,
                "message_id": outcome.message_id,
            })))
        } else {
            let kind = outcome.error_kind.unwrap_or(SendErrorKind::Unknown);
            Ok(ToolOutput::error(
                format!(
                    "failed to send email ({kind:?}){}",
                    if outcome.retryable { " — retryable" } else { " — not retryable" }
                ),
                outcome.retryable,
            ))
        }
    }
}

/// Reads `input[field]` as either a single address string or an array of
/// address strings, trimming and dropping empty entries. Absent is treated
/// as an empty list (the caller decides whether that's an error).
fn read_address_list(input: &Value, field: &str) -> Result<Vec<String>, String> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => {
            let s = s.trim();
            Ok(if s.is_empty() { Vec::new() } else { vec![s.to_string()] })
        }
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let s = item.as_str().ok_or_else(|| format!("'{field}' array entries must be strings"))?;
                let s = s.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
            }
            Ok(out)
        }
        Some(_) => Err(format!("'{field}' must be a string or an array of strings")),
    }
}

/// Picks the Email binding to send from: the explicitly requested
/// `binding_id` if given, or the agent's single enabled Email binding.
/// Errors clearly rather than guessing when there's none or more than one.
fn resolve_email_binding<'a>(
    profile: &'a AgentProfile,
    requested_binding_id: Option<&str>,
) -> Result<&'a ChannelBinding, String> {
    if let Some(binding_id) = requested_binding_id {
        return profile
            .channels
            .iter()
            .find(|b| b.binding_id == binding_id && b.kind == ChannelKind::Email)
            .ok_or_else(|| format!("no Email binding with id '{binding_id}' on this agent"));
    }

    let mut enabled = profile.channels.iter().filter(|b| b.kind == ChannelKind::Email && b.enabled);
    let first = enabled.next();
    if enabled.next().is_some() {
        return Err(
            "this agent has more than one enabled Email binding — pass 'binding_id' to select one".to_string(),
        );
    }
    first.ok_or_else(|| "this agent has no enabled Email binding configured".to_string())
}

/// Ensures `subject` starts with "Re: " exactly once (case-insensitive check
/// against an existing prefix, so a subject a sender already prefixed isn't
/// doubled to "Re: Re: ...").
fn ensure_re_prefixed(subject: &str) -> String {
    if subject.trim_start().to_ascii_lowercase().starts_with("re:") {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    }
}

/// Normalizes a caller-supplied Message-ID into the RFC5322 `<...>`-wrapped
/// form regardless of whether the caller already wrapped it — inbound emails
/// surface the id unwrapped (see `channels::email::ingest`), but a caller
/// might paste it either way.
fn normalize_message_id(raw: &str) -> String {
    let trimmed = raw.trim().trim_start_matches('<').trim_end_matches('>');
    format!("<{trimmed}>")
}

/// Generates a fresh `<...>`-wrapped Message-ID for an outbound email, using
/// the sending address's domain so the id is traceable to the sending
/// account without depending on server-assigned ids.
fn generate_message_id(from_address: &str) -> String {
    let domain = from_address.rsplit_once('@').map(|(_, d)| d).unwrap_or("localhost");
    format!("<{}@{}>", uuid::Uuid::new_v4(), domain)
}
