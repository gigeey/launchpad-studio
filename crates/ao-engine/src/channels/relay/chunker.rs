//! Message-length chunker shared by every synchronous chat channel's
//! outbound relay: splits a reply into pieces no longer than a
//! caller-supplied `char` limit, preferring to break at the last newline
//! within a chunk so a reply doesn't get cut mid-sentence, and falling back
//! to a hard character cut when a single line exceeds the limit on its own.
//!
//! Telegram's `TELEGRAM_MAX_MESSAGE_CHARS` (4096) and Discord's
//! `DISCORD_CHUNK_THRESHOLD_CHARS` (1900) differ only in the limit value,
//! never the splitting policy — hence a single algorithm parameterized by
//! `limit` rather than two near-identical copies.

/// Splits `text` into chunks no longer than `limit` `char`s each. `limit` is
/// measured in `char`s, not necessarily the unit a channel's own API caps on
/// (e.g. Telegram's documented limit is in UTF-16 code units) — the same
/// approximation both channels' original, per-channel chunkers already made.
///
/// KNOWN LIMITATION: this chunks whatever `text` a caller hands it, at face
/// value. If a caller chunks *before* converting to a richer wire format —
/// Telegram does exactly this: it chunks the original markdown reply, then
/// converts each chunk to HTML — the limit is enforced against the
/// pre-conversion length, not the post-conversion size the channel's API
/// actually caps on. HTML markup inflates length, so a chunk right at the
/// limit can still exceed the channel's real cap after conversion. That's
/// pre-existing caller behavior this extraction preserves verbatim; fixing
/// it would require the chunker to know about wire-format inflation, which
/// is out of scope here — zero behavior change is the gate for this pass.
pub(crate) fn chunk_text(text: &str, limit: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            chunks.push(rest);
            break;
        }

        let hard_boundary = rest.char_indices().nth(limit).map(|(i, _)| i).unwrap_or(rest.len());
        let split_at = match rest[..hard_boundary].rfind('\n') {
            Some(0) | None => hard_boundary,
            Some(i) => i + 1,
        };

        chunks.push(&rest[..split_at]);
        rest = &rest[split_at..];
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_one_chunk_when_under_the_limit() {
        let chunks = chunk_text("hello from the agent", 4096);
        assert_eq!(chunks, vec!["hello from the agent"]);
    }

    #[test]
    fn respects_an_arbitrary_injected_limit() {
        // Neither Telegram's 4096 nor Discord's 1900 — proves the limit is a
        // genuine parameter, not a hidden per-channel constant.
        let limit = 37;
        let text = "x".repeat(200);
        let chunks = chunk_text(&text, limit);
        assert!(chunks.len() > 1, "expected more than one chunk");
        for chunk in &chunks {
            assert!(chunk.chars().count() <= limit);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn prefers_the_last_newline_boundary_within_the_limit() {
        let limit = 250;
        let line = "x".repeat(50);
        let text = std::iter::repeat(line.as_str()).take(20).collect::<Vec<_>>().join("\n");
        assert!(text.chars().count() > limit);

        let chunks = chunk_text(&text, limit);
        assert!(chunks.len() > 1, "expected more than one chunk");
        for chunk in &chunks {
            assert!(chunk.chars().count() <= limit);
        }
        assert_eq!(chunks.concat(), text, "chunking must preserve order and content");
        // Every non-final chunk should end right after a newline — proof the
        // split preferred the newline boundary over a hard cut.
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(chunk.ends_with('\n'), "non-final chunk {chunk:?} should end at a newline boundary");
        }
    }

    #[test]
    fn hard_cuts_a_single_line_that_exceeds_the_limit_with_no_newline_boundary() {
        let limit = 100;
        let text = "y".repeat(limit * 2 + 10);
        let chunks = chunk_text(&text, limit);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= limit);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn empty_text_produces_no_chunks() {
        let chunks = chunk_text("", 4096);
        assert!(chunks.is_empty());
    }
}
