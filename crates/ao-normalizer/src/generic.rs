use ao_protocol::event::AgentEventPayload;

use crate::traits::OutputNormalizer;

/// Generic normalizer that treats all output as plain text.
/// This is the fallback normalizer for any CLI that doesn't have a specialized implementation.
pub struct GenericNormalizer {
    accumulated: String,
}

impl GenericNormalizer {
    pub fn new() -> Self {
        Self {
            accumulated: String::new(),
        }
    }
}

impl Default for GenericNormalizer {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputNormalizer for GenericNormalizer {
    fn process_chunk(&mut self, chunk: &str) -> Vec<AgentEventPayload> {
        self.accumulated.push_str(chunk);
        vec![AgentEventPayload::TextDelta {
            text: chunk.to_string(),
        }]
    }

    fn finalize(&mut self, _exit_code: Option<i32>, stderr: &str) -> Vec<AgentEventPayload> {
        let mut events = Vec::new();

        if !self.accumulated.is_empty() {
            events.push(AgentEventPayload::TextComplete {
                text: std::mem::take(&mut self.accumulated),
            });
        }

        if !stderr.is_empty() {
            events.push(AgentEventPayload::Error {
                message: stderr.to_string(),
                recoverable: false,
            });
        }

        events
    }

    fn extract_session_id(&self) -> Option<String> {
        None
    }
}
