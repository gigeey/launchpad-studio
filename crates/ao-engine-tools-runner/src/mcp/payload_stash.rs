//! Bounded, in-memory side channel that stashes the typed MCP payload just
//! before [`super::adapter::mcp_result_to_tool_output`] flattens it into a
//! single text-bearing [`ToolOutput`](ao_engine_tools_core::output::ToolOutput).
//!
//! MCP servers return two distinct pieces of data on a `tools/call`: a
//! `content` array of blocks and, optionally, a top-level `structuredContent`
//! JSON value. The adapter collapses both into a single string for the model
//! (a provider-agnostic requirement — providers only understand text/image
//! blocks, not a typed side value). That collapse is one-way: once the JSON
//! `Value` becomes a pretty-printed string embedded in a text block, nothing
//! downstream can recover the original structure.
//!
//! This module keeps one extra copy of the pre-flatten value around, keyed by
//! the call that produced it, so a future consumer can look up the typed
//! payload without re-parsing text. It is deliberately inert: nothing reads
//! from the stash yet, and writing to it never changes what the adapter
//! returns to the model.
//!
//! ## Ownership
//!
//! The stash lives behind a process-wide [`OnceLock`] singleton ([`global`])
//! rather than as a field threaded through [`McpManager`](super::manager::McpManager)
//! and [`McpToolAdapter`](super::adapter::McpToolAdapter). Threading an
//! `Arc<PayloadStash>` through cleanly would require changing the
//! `McpToolAdapter` constructor signature, which has call sites across both
//! `manager.rs` and this crate's own adapter test suite — well past the
//! handful of sites a POC-stage change should touch. The free functions in
//! `mcp_result_to_tool_output` can reach the global directly with no
//! constructor or struct changes at all. Revisit this once a real consumer
//! needs the stash injected (e.g. for per-session isolation or eviction
//! tuning) rather than a single process-wide pool.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Hard cap on the number of entries the stash retains. Once reached, the
/// oldest entry is evicted before the new one is appended, so the stash can
/// never grow unbounded over a long-running process.
const MAX_ENTRIES: usize = 256;

/// Hard cap on how large `text` may be before [`StashedPayload::json_body`]
/// will even attempt to parse it as JSON. Guards against re-running a parse
/// over a pathologically large text blob on every single poll — this stash
/// has no existing size bound on `text` itself (a server can return
/// arbitrarily long prose), so `json_body` enforces its own.
const MAX_TEXT_BYTES_FOR_JSON_RESCUE: usize = 4 * 1024 * 1024;

/// One MCP tool call's raw, pre-flatten result.
///
/// `structured` and `text` mirror the two pieces
/// [`mcp_result_to_tool_output`](super::adapter::mcp_result_to_tool_output)
/// combines into a single string: the top-level `structuredContent` value
/// (untouched — no `to_string_pretty` applied) and the plain-text content
/// blocks joined with `"\n"`. Either may be absent depending on what the
/// server returned; both absent means there was nothing worth stashing.
///
/// `args` is the raw call arguments the payload was produced from, kept
/// alongside `args_hash` (a one-way digest) so a caller can recover the
/// exact request that led to this response — e.g. to replay the same call
/// later without needing a model to reconstruct the arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct StashedPayload {
    pub server: String,
    pub tool: String,
    pub args: Value,
    pub args_hash: String,
    pub captured_at: DateTime<Utc>,
    pub structured: Option<Value>,
    pub text: Option<String>,
}

impl StashedPayload {
    /// The best JSON body this payload can offer for extraction purposes,
    /// preferring the server's own `structuredContent` and falling back to
    /// parsing `text` as JSON when that's all a server ever sent.
    ///
    /// Many MCP servers never populate `structuredContent` at all — the MCP
    /// spec made it optional and added it late — and instead return a
    /// `JSON.stringify(...)`-style blob inside a plain text content block.
    /// Without this fallback, every one of those servers' watches would be
    /// stuck paying for a full model session on every poll forever, purely
    /// because of how a server happened to shape its response rather than
    /// anything about the content itself. This method rescues that case by
    /// attempting to parse `text` as JSON when `structured` is absent.
    ///
    /// The rescue only succeeds when the parsed value is a `Value::Array` or
    /// `Value::Object` — a bare scalar (number/bool/null/string) isn't
    /// something a selector can enumerate paths over or index into the way a
    /// real structured payload can, so it's treated the same as no body at
    /// all. A parse failure (the text isn't JSON — HTML, prose, etc.) is
    /// likewise `None`, not an error; callers already treat "no usable body"
    /// as the signal to fall back further.
    ///
    /// Cost guard: if `text` exceeds [`MAX_TEXT_BYTES_FOR_JSON_RESCUE`], the
    /// parse is skipped entirely and this returns `None` — a poll must not
    /// re-run a full JSON parse over a pathologically large text blob every
    /// time it fires.
    ///
    /// **Important:** a body recovered this way did NOT come from the
    /// server's own `structuredContent` field, so it must never be treated
    /// as server-declared structured content for [`Tier`](ao_protocol) —
    /// i.e. never wire this method's `is_some()` into `infer_tier`'s
    /// `has_structured_content` argument. Callers that need to distinguish
    /// "real" structured content from a text-parsed rescue for tier purposes
    /// should keep checking `self.structured.is_some()` directly and use
    /// this method only for the extraction body itself.
    pub fn json_body(&self) -> Option<Cow<'_, Value>> {
        if let Some(structured) = self.structured.as_ref() {
            return Some(Cow::Borrowed(structured));
        }
        let text = self.text.as_ref()?;
        if text.len() > MAX_TEXT_BYTES_FOR_JSON_RESCUE {
            return None;
        }
        match serde_json::from_str::<Value>(text) {
            Ok(parsed @ (Value::Array(_) | Value::Object(_))) => Some(Cow::Owned(parsed)),
            _ => None,
        }
    }
}

/// Bounded, in-memory store of [`StashedPayload`] entries keyed by
/// `(server, tool, args_hash)`.
///
/// Repeated calls with the same key overwrite the previous entry — the
/// stash tracks the latest result per call signature, not a history. Entries
/// are otherwise kept in insertion order so the oldest can be evicted first
/// once [`MAX_ENTRIES`] is reached.
pub struct PayloadStash {
    entries: Mutex<VecDeque<StashedPayload>>,
}

impl Default for PayloadStash {
    fn default() -> Self {
        Self::new()
    }
}

impl PayloadStash {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
        }
    }

    /// Record `entry`, overwriting any existing entry with the same
    /// `(server, tool, args_hash)` key. Evicts the oldest entry first if the
    /// stash is at capacity after the insert.
    pub fn record(&self, entry: StashedPayload) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|e| {
            !(e.server == entry.server && e.tool == entry.tool && e.args_hash == entry.args_hash)
        });
        entries.push_back(entry);
        while entries.len() > MAX_ENTRIES {
            entries.pop_front();
        }
    }

    /// Look up the entry for an exact call signature.
    pub fn get(&self, server: &str, tool: &str, args_hash: &str) -> Option<StashedPayload> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .find(|e| e.server == server && e.tool == tool && e.args_hash == args_hash)
            .cloned()
    }

    /// Look up the most recently recorded entry for a `(server, tool)` pair,
    /// regardless of `args_hash`.
    pub fn latest_for(&self, server: &str, tool: &str) -> Option<StashedPayload> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .rev()
            .find(|e| e.server == server && e.tool == tool)
            .cloned()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// The process-wide [`PayloadStash`] singleton.
///
/// See the module-level "Ownership" section for why this is a process
/// global rather than a field on [`McpManager`](super::manager::McpManager).
pub fn global() -> &'static PayloadStash {
    static STASH: OnceLock<PayloadStash> = OnceLock::new();
    STASH.get_or_init(PayloadStash::new)
}

/// Stable hash of a tool call's arguments, suitable for use as the
/// `args_hash` component of a stash key.
///
/// Object keys are sorted recursively before hashing so that two
/// semantically identical argument sets with different key order (which
/// `serde_json` may produce depending on how the value was built) always
/// hash the same.
pub fn hash_args(args: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(args).as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Render `value` as a JSON string with object keys sorted recursively at
/// every level, so the output is independent of the original key order.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = String::from("{");
            for (i, (key, val)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                out.push_str(&canonical_json(val));
            }
            out.push('}');
            out
        }
        Value::Array(items) => {
            let mut out = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_json(item));
            }
            out.push(']');
            out
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(server: &str, tool: &str, args_hash: &str, tag: &str) -> StashedPayload {
        StashedPayload {
            server: server.to_string(),
            tool: tool.to_string(),
            args: serde_json::json!({ "tag": tag }),
            args_hash: args_hash.to_string(),
            captured_at: Utc::now(),
            structured: Some(serde_json::json!({ "tag": tag })),
            text: Some(format!("text-{tag}")),
        }
    }

    // ── canonicalization / hashing ───────────────────────────────────────────

    #[test]
    fn hash_args_stable_across_key_order() {
        let a = serde_json::json!({ "a": 1, "b": 2 });
        let b = serde_json::json!({ "b": 2, "a": 1 });
        assert_eq!(hash_args(&a), hash_args(&b));
    }

    #[test]
    fn hash_args_stable_across_nested_key_order() {
        let a = serde_json::json!({ "outer": { "x": 1, "y": 2 }, "list": [{"p": 1, "q": 2}] });
        let b = serde_json::json!({ "list": [{"q": 2, "p": 1}], "outer": { "y": 2, "x": 1 } });
        assert_eq!(hash_args(&a), hash_args(&b));
    }

    #[test]
    fn hash_args_differs_for_different_values() {
        let a = serde_json::json!({ "a": 1 });
        let b = serde_json::json!({ "a": 2 });
        assert_ne!(hash_args(&a), hash_args(&b));
    }

    // ── roundtrip / overwrite ────────────────────────────────────────────────

    #[test]
    fn stash_then_get_roundtrips() {
        let stash = PayloadStash::new();
        let entry = sample("srv", "tool", "hash1", "v1");
        stash.record(entry.clone());

        let got = stash.get("srv", "tool", "hash1");
        assert_eq!(got, Some(entry));
    }

    #[test]
    fn get_returns_none_for_unknown_key() {
        let stash = PayloadStash::new();
        stash.record(sample("srv", "tool", "hash1", "v1"));
        assert_eq!(stash.get("srv", "tool", "other-hash"), None);
        assert_eq!(stash.get("other-srv", "tool", "hash1"), None);
    }

    /// The whole point of keying `get` on `args_hash`: two calls to the same
    /// `(server, tool)` with different arguments must not cross-contaminate.
    /// Without this, a scheduled poll could read back a payload produced by
    /// a concurrent assignment or an unrelated chat session that happened to
    /// hit the same connector tool.
    #[test]
    fn get_returns_none_when_args_hash_differs_but_server_and_tool_match() {
        let stash = PayloadStash::new();
        stash.record(sample("srv", "tool", "hash-from-session-a", "a"));
        assert_eq!(stash.get("srv", "tool", "hash-from-session-b"), None);
    }

    #[test]
    fn record_then_get_roundtrips_raw_args() {
        let stash = PayloadStash::new();
        let args = serde_json::json!({ "query": "status", "limit": 5 });
        let entry = StashedPayload {
            server: "srv".to_string(),
            tool: "tool".to_string(),
            args: args.clone(),
            args_hash: hash_args(&args),
            captured_at: Utc::now(),
            structured: Some(serde_json::json!({ "ok": true })),
            text: Some("done".to_string()),
        };
        stash.record(entry);

        let got = stash.get("srv", "tool", &hash_args(&args)).expect("entry should be recorded");
        assert_eq!(got.args, args, "raw args must round-trip byte-for-byte");
    }

    #[test]
    fn repeat_key_overwrites_last_write_wins() {
        let stash = PayloadStash::new();
        stash.record(sample("srv", "tool", "hash1", "old"));
        stash.record(sample("srv", "tool", "hash1", "new"));

        assert_eq!(stash.len(), 1, "overwrite must not grow the entry count");
        let got = stash.get("srv", "tool", "hash1").unwrap();
        assert_eq!(got.text.as_deref(), Some("text-new"));
    }

    #[test]
    fn latest_for_ignores_args_hash_and_returns_most_recent() {
        let stash = PayloadStash::new();
        stash.record(sample("srv", "tool", "hash1", "first"));
        stash.record(sample("srv", "tool", "hash2", "second"));

        let got = stash.latest_for("srv", "tool").unwrap();
        assert_eq!(got.text.as_deref(), Some("text-second"));
    }

    #[test]
    fn latest_for_none_when_no_entries_for_pair() {
        let stash = PayloadStash::new();
        stash.record(sample("srv", "tool", "hash1", "v1"));
        assert_eq!(stash.latest_for("srv", "other_tool"), None);
    }

    // ── eviction ─────────────────────────────────────────────────────────────

    #[test]
    fn eviction_caps_at_max_entries_oldest_first() {
        let stash = PayloadStash::new();
        for i in 0..(MAX_ENTRIES + 10) {
            stash.record(sample("srv", "tool", &format!("hash{i}"), &format!("v{i}")));
        }

        assert_eq!(stash.len(), MAX_ENTRIES, "stash must never exceed the hard cap");

        // The first 10 keys were evicted to make room for the newest 10.
        for i in 0..10 {
            assert_eq!(
                stash.get("srv", "tool", &format!("hash{i}")),
                None,
                "entry {i} should have been evicted"
            );
        }
        // The most recent entry must survive.
        let last = MAX_ENTRIES + 9;
        assert!(stash.get("srv", "tool", &format!("hash{last}")).is_some());
    }

    #[test]
    fn overwrite_does_not_trigger_eviction_of_others() {
        let stash = PayloadStash::new();
        stash.record(sample("srv", "tool", "hash0", "a"));
        stash.record(sample("srv", "tool", "hash1", "b"));
        // Overwrite hash0 — total distinct keys stays at 2, well under the cap.
        stash.record(sample("srv", "tool", "hash0", "a-updated"));

        assert_eq!(stash.len(), 2);
        assert!(stash.get("srv", "tool", "hash1").is_some());
        assert_eq!(
            stash.get("srv", "tool", "hash0").unwrap().text.as_deref(),
            Some("text-a-updated")
        );
    }

    // ── global singleton ─────────────────────────────────────────────────────

    #[test]
    fn global_returns_same_instance_and_is_reachable() {
        // Use a key unlikely to collide with any other test in this binary
        // touching the same process-global stash.
        global().record(sample(
            "payload_stash_tests__global_srv",
            "global_tool",
            "global_hash",
            "global",
        ));
        let got = global().get("payload_stash_tests__global_srv", "global_tool", "global_hash");
        assert!(got.is_some(), "value recorded via global() must be readable via global()");
    }

    // ── json_body ────────────────────────────────────────────────────────────

    fn payload_with(structured: Option<Value>, text: Option<&str>) -> StashedPayload {
        StashedPayload {
            server: "srv".to_string(),
            tool: "tool".to_string(),
            args: serde_json::json!({}),
            args_hash: "hash".to_string(),
            captured_at: Utc::now(),
            structured,
            text: text.map(str::to_string),
        }
    }

    #[test]
    fn json_body_prefers_structured_when_present() {
        let structured = serde_json::json!({ "items": [1, 2, 3] });
        let payload = payload_with(Some(structured.clone()), Some("this text must be ignored"));
        assert_eq!(payload.json_body().as_deref(), Some(&structured));
    }

    #[test]
    fn json_body_rescues_a_json_array_from_text() {
        let payload = payload_with(None, Some(r#"[{"id":"a"},{"id":"b"}]"#));
        let expected = serde_json::json!([{ "id": "a" }, { "id": "b" }]);
        assert_eq!(payload.json_body().as_deref(), Some(&expected));
    }

    #[test]
    fn json_body_rescues_a_json_object_from_text() {
        let payload = payload_with(None, Some(r#"{"items":[{"id":"a"}]}"#));
        let expected = serde_json::json!({ "items": [{ "id": "a" }] });
        assert_eq!(payload.json_body().as_deref(), Some(&expected));
    }

    #[test]
    fn json_body_rejects_scalar_json_in_text() {
        for scalar in ["42", "true", "null", "\"hi\""] {
            let payload = payload_with(None, Some(scalar));
            assert_eq!(payload.json_body(), None, "scalar {scalar} must not be treated as a usable body");
        }
    }

    #[test]
    fn json_body_none_for_non_json_text() {
        let payload = payload_with(None, Some("<html><body>not json</body></html>"));
        assert_eq!(payload.json_body(), None);
    }

    #[test]
    fn json_body_none_when_both_fields_absent() {
        let payload = payload_with(None, None);
        assert_eq!(payload.json_body(), None);
    }

    #[test]
    fn json_body_none_for_oversized_text_even_if_valid_json() {
        // A valid, well-formed JSON array (trailing whitespace is legal
        // after the top-level value) that just happens to exceed the
        // parse-attempt cap — must be skipped on size alone, not because it
        // fails to parse.
        let huge_text = format!("[1,2,3]{}", " ".repeat(MAX_TEXT_BYTES_FOR_JSON_RESCUE));
        let payload = payload_with(None, Some(&huge_text));
        assert_eq!(payload.json_body(), None);
    }
}
