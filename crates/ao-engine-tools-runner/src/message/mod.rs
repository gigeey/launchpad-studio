//! Canonical message types for the runner's provider seam.
//!
//! [`Message`] and [`ContentBlock`] are the normalisation target that every
//! provider crate maps to and from. Provider-specific wire formats (Anthropic
//! `content` arrays, OpenAI `messages`, Gemini `contents`) stay inside their
//! respective provider crates; only canonical [`Message`]s cross the runner
//! boundary.
//!
//! # Dual `ToolResult` levels
//!
//! [`Message::ToolResult`] is the *transcript-level* shape: one logical message
//! per tool round-trip, carrying `tool_use_id` at the message level. The query
//! loop appends one `Message::ToolResult` per tool call after execution.
//!
//! [`ContentBlock::ToolResult`] is the *block-level* shape for provider formats
//! (such as Anthropic) that permit `tool_result` blocks interleaved with text
//! inside a `user`-role content array. Most callers will never construct
//! `ContentBlock::ToolResult` directly — normalizer impls produce it during
//! deserialisation when the provider's wire format requires the inline shape.
//!
//! This dual representation is intentional. Collapsing to a single level would
//! force one of the two real-world cases into a contortion.
//!
//! # Media content blocks
//!
//! `ContentBlock::Image` and `ContentBlock::Document` carry base64-encoded
//! media a tool returned (e.g. a screenshot or a PDF) so it can be delivered to
//! the model. They only ever appear inside a [`Message::ToolResult`] content
//! array; the query loop builds them from a [`ToolOutput::Blocks`] payload.
//! Each provider normalizer maps them to its own wire shape — Anthropic embeds
//! `image`/`document` blocks directly in the `tool_result` content array,
//! Gemini splits them into `inlineData` parts in the enclosing user-role
//! message, and OpenAI (whose tool-role messages cannot carry media) downgrades
//! them to a text placeholder.
//!
//! # Thinking blocks and multi-turn legality
//!
//! [`ContentBlock::Thinking`] carries the model's reasoning text plus the
//! provider's cryptographic signature. Anthropic's Messages API requires that
//! when extended thinking is enabled and the assistant emitted a `thinking`
//! block alongside any `tool_use`, the *next* turn's transcript must echo that
//! `thinking` block back verbatim — including the signature — before the
//! tool_result blocks. The runner threads thinking blocks into the assistant
//! turn it builds at the end of each iteration so multi-turn tool-using
//! sessions stay legal under the API's continuity rule.
//!
//! [`ContentBlock::RedactedThinking`] is the same rule for reasoning the
//! provider chose to withhold: the plaintext is replaced by an opaque
//! encrypted `data` blob, but the block must still be replayed verbatim
//! alongside (and in the original order with) any signed `thinking` blocks
//! from the same turn.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod normalizer;
pub use normalizer::{MessageNormalizer, NormalizerError};

#[cfg(test)]
mod tests;

/// Canonical message in the runner's provider-agnostic transcript.
///
/// Variants map to the four logical roles in a multi-turn conversation:
/// - [`Message::System`] — static system prompt injected before the first turn.
/// - [`Message::User`] — human-authored content (text and/or inline tool-result blocks).
/// - [`Message::Assistant`] — model-authored content (text and/or tool-use blocks).
/// - [`Message::ToolResult`] — the runner's response to a [`ContentBlock::ToolUse`]
///   emitted by the assistant. Kept as a first-class variant so the query loop's
///   bookkeeping is uniform without scanning content arrays for the role hint.
///
/// Serialised with an internal `"role"` tag so each JSON object carries its role
/// inline: `{"role": "assistant", "content": [...]}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// Static system-level instruction. Typically injected once before the
    /// first user turn; providers that require a separate system-prompt field
    /// (rather than a message in the array) handle extraction in their normalizer.
    System { content: String },

    /// A turn authored by the human side of the conversation. Content may
    /// include text and inline tool-result blocks (produced when the model's
    /// prior turn included tool-use blocks the runner executed).
    User { content: Vec<ContentBlock> },

    /// A turn authored by the model. Content may include text and tool-use
    /// blocks; tool-use blocks trigger tool execution by the runner.
    Assistant { content: Vec<ContentBlock> },

    /// The runner's response to one tool-use block from the assistant. Kept
    /// at the message level (not nested inside a `User` content array) so
    /// the query loop can build the transcript uniformly. Provider normalizers
    /// group consecutive `ToolResult` messages into a single wire-level
    /// `user`-role message when the provider requires that shape.
    ToolResult {
        /// The `id` from the [`ContentBlock::ToolUse`] this result answers.
        tool_use_id: String,
        /// Result payload, one or more content blocks.
        content: Vec<ContentBlock>,
        /// `true` if the tool execution produced an error that the model
        /// should receive as a failure signal.
        is_error: bool,
    },
}

/// A single typed block within a [`Message`]'s content array.
///
/// All provider wire formats decompose message content into blocks of these
/// types; the canonical `ContentBlock` is the normalisation target.
///
/// Serialised with an internal `"type"` tag: `{"type": "text", "text": "..."}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text content.
    Text { text: String },

    /// A tool call the model wants the runner to execute. `id` is the opaque
    /// call identifier echoed back in the matching [`ContentBlock::ToolResult`]
    /// or [`Message::ToolResult`]. `input` is the raw, unvalidated argument JSON.
    ToolUse {
        /// Opaque call identifier. Must be echoed in the matching result.
        id: String,
        /// Registered tool name.
        name: String,
        /// Unvalidated argument JSON. Validation occurs in the runner's
        /// validation layer before the tool is invoked.
        input: Value,
    },

    /// A tool result block for providers that embed results inside a `user`-role
    /// content array. Most callers construct [`Message::ToolResult`] instead;
    /// normalizer impls produce this variant during deserialisation when the
    /// provider requires the inline shape.
    ToolResult {
        /// The `id` from the [`ContentBlock::ToolUse`] this block answers.
        tool_use_id: String,
        /// String-encoded result payload.
        content: String,
        /// `true` if the tool execution produced an error.
        is_error: bool,
    },

    /// A reasoning block emitted by the assistant when extended thinking is
    /// enabled. Carries the (possibly empty) reasoning text plus the
    /// provider's cryptographic `signature`. The signature is what makes
    /// multi-turn replay legal: Anthropic's API rejects a follow-up turn
    /// whose transcript echoes a `thinking` block without the original
    /// signature when the prior turn also emitted `tool_use`. Treating both
    /// fields as `Option<String>` lets normalizers absorb the
    /// `display = "omitted"` case (no text on the wire — only the
    /// signature) and any future provider variant that surfaces text without
    /// a signature, without forcing a contortion at the call site.
    Thinking {
        /// Reasoning text. `None` when the provider suppressed it
        /// (e.g. `display = "omitted"`); otherwise the concatenation of all
        /// `thinking_delta` chunks for the block.
        text: Option<String>,
        /// Provider attestation that the reasoning was actually performed.
        /// Anthropic emits this verbatim and requires it on replay; other
        /// providers may not produce one, in which case it stays `None`.
        signature: Option<String>,
    },

    /// A reasoning block whose contents the provider withheld for safety
    /// reasons, returning an opaque encrypted payload in place of plaintext
    /// `thinking`. There is no human-readable text and no separate
    /// signature — the `data` blob is the whole block. It still counts as a
    /// reasoning block for the multi-turn continuity rule: a transcript that
    /// carries any `tool_use` from a turn must echo every reasoning block
    /// that turn produced, redacted ones included, or the follow-up request
    /// is rejected. The blob is round-tripped verbatim and never inspected
    /// or rendered.
    RedactedThinking {
        /// Opaque, provider-encrypted reasoning payload. Echoed back
        /// byte-for-byte on the next turn; never decoded on our side.
        data: String,
    },

    /// A base64-encoded image returned by a tool. Appears inside a
    /// [`Message::ToolResult`] content array. Anthropic accepts this directly
    /// in `tool_result.content`; Gemini carries it as an `inlineData` part in
    /// the enclosing user-role message; OpenAI tool messages cannot hold media,
    /// so its normalizer downgrades the block to a text placeholder.
    Image {
        /// Image MIME type, e.g. `image/png`.
        media_type: String,
        /// Base64-encoded image bytes.
        data: String,
    },

    /// A base64-encoded document (PDF) returned by a tool. Appears inside a
    /// [`Message::ToolResult`] content array. Anthropic accepts this directly
    /// in `tool_result.content`; Gemini carries it as an `inlineData` part;
    /// OpenAI has no document channel and downgrades to a text placeholder.
    Document {
        /// Document MIME type — `application/pdf` today.
        media_type: String,
        /// Base64-encoded document bytes.
        data: String,
        /// Optional display title; providers that support it surface it in
        /// their UI, others ignore it.
        title: Option<String>,
    },
}

/// Strip `Thinking`/`RedactedThinking` blocks from assistant turns that are
/// *not* part of an active tool-use cycle.
///
/// # Why this exists
///
/// Anthropic's Messages API validates the cryptographic signatures of the
/// reasoning blocks in the **most-recent assistant message** of every request
/// and rejects the call when they differ from what the model originally
/// emitted:
///
/// ```text
/// messages.N.content.M: thinking or redacted_thinking blocks in the latest
/// assistant message cannot be modified.
/// ```
///
/// Reasoning blocks only carry continuity value while a tool-use cycle is in
/// flight — an assistant turn that emitted `tool_use` and is answered by
/// `tool_result` message(s) on the following turn. Once a plain user turn (or
/// the end of the transcript) closes the assistant turn, its reasoning blocks
/// serve no purpose, and replaying a reconstructed copy that isn't byte-perfect
/// (e.g. rebuilt from a persisted transcript) trips the 400 above.
///
/// # Rule
///
/// Keep reasoning blocks on an assistant message **only when the message
/// immediately following it is a [`Message::ToolResult`]** — that is exactly
/// the active-cycle shape. Strip them from every other assistant message. An
/// assistant message left with no content by the strip is removed entirely so
/// it never serialises to an empty `content: []` (which the API also rejects).
///
/// Applying this at a provider seam is safe for the live in-session loop: while
/// a tool cycle runs, each assistant turn is followed by its `tool_result`
/// messages and is therefore preserved verbatim; only closed turns lose their
/// (now-superfluous) reasoning.
pub fn strip_closed_turn_reasoning(messages: &mut Vec<Message>) {
    let keep: Vec<bool> = (0..messages.len())
        .map(|i| {
            matches!(messages[i], Message::Assistant { .. })
                && matches!(messages.get(i + 1), Some(Message::ToolResult { .. }))
        })
        .collect();

    for (i, msg) in messages.iter_mut().enumerate() {
        if keep[i] {
            continue;
        }
        if let Message::Assistant { content } = msg {
            content.retain(|b| {
                !matches!(
                    b,
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                )
            });
        }
    }

    // Drop assistant turns emptied by the strip (a reasoning-only turn with no
    // text or tool_use). Such a turn never carries a tool_use, so removing it
    // cannot orphan a tool_result.
    messages.retain(|m| !matches!(m, Message::Assistant { content } if content.is_empty()));
}

/// Trim the oldest conversation history to fit an approximate token budget,
/// enforced client-side before a request ever reaches the wire.
///
/// Neither Anthropic's Messages API nor OpenAI's Chat Completions API expose
/// a "cap total context tokens" request parameter — both only cap *output*
/// tokens (`max_tokens` / `max_completion_tokens`). Keeping a conversation
/// under an operator-chosen context budget is therefore this crate's job,
/// not the provider's; both provider request builders call this function on
/// `request.messages` before building the wire body.
///
/// `max_context_tokens` of `None` means "no cap" — the input is returned
/// unchanged (a cheap, allocation-light path for the common case).
///
/// The token count is a coarse chars/4 heuristic, not exact tokenization —
/// good enough to fail toward trimming a little early rather than risking a
/// request that blows the model's real context window. It does not include
/// the system prompt, which callers carry separately from `messages`.
///
/// Messages are dropped from the oldest end in atomic groups, never
/// individually: a [`Message::ToolResult`] can never be separated from the
/// [`Message::Assistant`] tool-use turn it answers (Anthropic's API rejects
/// a `tool_result` with no matching `tool_use` in the same request), so each
/// group is one non-`ToolResult` message plus every `ToolResult` message
/// immediately following it. At least the single most recent group is
/// always kept, even if it alone exceeds the budget — this function trims
/// history, it never manufactures an empty request.
pub fn truncate_to_context_budget(messages: &[Message], max_context_tokens: Option<u32>) -> Vec<Message> {
    let Some(budget) = max_context_tokens else {
        return messages.to_vec();
    };
    let budget = budget as u64;

    // Group into atomic (non-splittable) units: a leading non-ToolResult
    // message followed by any contiguous run of ToolResult messages.
    let mut groups: Vec<(u64, Vec<Message>)> = Vec::new();
    for msg in messages {
        let is_tool_result = matches!(msg, Message::ToolResult { .. });
        if is_tool_result {
            if let Some(last) = groups.last_mut() {
                last.0 += estimate_message_tokens(msg);
                last.1.push(msg.clone());
                continue;
            }
        }
        groups.push((estimate_message_tokens(msg), vec![msg.clone()]));
    }

    // Walk from the newest group backward, keeping whole groups while under
    // budget. The last group (even alone) is always kept regardless of size.
    let mut kept_from = groups.len();
    let mut total: u64 = 0;
    for (i, (tokens, _)) in groups.iter().enumerate().rev() {
        let next_total = total + tokens;
        if next_total > budget && kept_from != groups.len() {
            break;
        }
        total = next_total;
        kept_from = i;
    }

    groups
        .into_iter()
        .skip(kept_from)
        .flat_map(|(_, msgs)| msgs)
        .collect()
}

/// Coarse chars/4 token estimate for one [`Message`], plus a small
/// per-message constant for role/envelope overhead. Not exact tokenization —
/// see [`truncate_to_context_budget`] for why an approximation is
/// sufficient here.
fn estimate_message_tokens(message: &Message) -> u64 {
    const PER_MESSAGE_OVERHEAD: u64 = 4;
    let content_tokens: u64 = match message {
        Message::System { content } => estimate_text_tokens(content),
        Message::User { content } | Message::Assistant { content } => {
            content.iter().map(estimate_block_tokens).sum()
        }
        Message::ToolResult { content, .. } => content.iter().map(estimate_block_tokens).sum(),
    };
    PER_MESSAGE_OVERHEAD + content_tokens
}

fn estimate_block_tokens(block: &ContentBlock) -> u64 {
    // Flat estimate for media blocks: their base64 payload length is not a
    // meaningful proxy for the tokens a model actually spends on an image or
    // document, so counting raw bytes/4 would wildly overestimate.
    const MEDIA_BLOCK_ESTIMATE: u64 = 300;
    match block {
        ContentBlock::Text { text } => estimate_text_tokens(text),
        ContentBlock::ToolUse { name, input, .. } => {
            estimate_text_tokens(name) + estimate_text_tokens(&input.to_string())
        }
        ContentBlock::ToolResult { content, .. } => estimate_text_tokens(content),
        ContentBlock::Thinking { text, .. } => text.as_deref().map(estimate_text_tokens).unwrap_or(0),
        ContentBlock::RedactedThinking { data } => estimate_text_tokens(data),
        ContentBlock::Image { .. } | ContentBlock::Document { .. } => MEDIA_BLOCK_ESTIMATE,
    }
}

fn estimate_text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}
