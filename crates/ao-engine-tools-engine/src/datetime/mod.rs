mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use chrono::Utc;
use chrono_tz::Tz;
use serde_json::{json, Value};

/// Universal "what time is it?" primitive.
///
/// Resolution order for the local timezone:
/// 1. `RunnerContext::preferences` → `UserPreferences::timezone` if set
/// 2. System IANA timezone via `iana_time_zone::get_timezone()`
/// 3. UTC (used as both UTC and local when nothing else resolves)
///
/// Output is a single text block to keep tool-result rendering predictable
/// across runners. Format is human-readable but every line is mechanically
/// parseable if the model needs to grep it back out.
pub struct DateTime;

#[async_trait]
impl EngineTool for DateTime {
    fn name(&self) -> &str {
        "DateTime"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, _input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let now_utc: chrono::DateTime<Utc> = Utc::now();
        let (tz_name, local_iso) = resolve_local(ctx, now_utc).await;

        let body = format!(
            "Current time:\n- UTC: {utc}\n- Local ({tz}): {local}\n- Unix epoch: {epoch}",
            utc = now_utc.format("%Y-%m-%dT%H:%M:%SZ"),
            tz = tz_name,
            local = local_iso,
            epoch = now_utc.timestamp()
        );
        Ok(ToolOutput::text(body))
    }
}

/// Returns `(tz_name, local_iso_string)` for the resolved local timezone.
/// Falls back through preferences → system IANA → UTC.
async fn resolve_local(
    ctx: &RunnerContext,
    now_utc: chrono::DateTime<Utc>,
) -> (String, String) {
    // 1. User preferences override
    let candidate = pref_timezone(ctx).await;

    // 2. System IANA
    let candidate = candidate.or_else(|| iana_time_zone::get_timezone().ok());

    // 3. Try to parse whichever candidate we ended up with
    if let Some(name) = candidate {
        if let Ok(tz) = name.parse::<Tz>() {
            let local = now_utc.with_timezone(&tz);
            return (name, local.format("%Y-%m-%dT%H:%M:%S%:z").to_string());
        }
        // Name is set but doesn't parse — render UTC under the requested name
        // so the model still sees what was tried.
        return (
            name,
            now_utc.format("%Y-%m-%dT%H:%M:%S+00:00").to_string(),
        );
    }

    // 4. Pure UTC fallback
    (
        "UTC".to_string(),
        now_utc.format("%Y-%m-%dT%H:%M:%S+00:00").to_string(),
    )
}

async fn pref_timezone(ctx: &RunnerContext) -> Option<String> {
    let store = ctx.preferences.as_ref()?;
    let prefs = store.get().await.ok().flatten()?;
    prefs.timezone
}
