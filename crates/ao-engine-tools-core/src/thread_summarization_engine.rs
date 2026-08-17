use async_trait::async_trait;

/// All inputs assembled by `SummarizeThread` before calling the engine.
///
/// The engine receives no live conversation context of its own — only the
/// target thread's own transcript (already formatted into plain text) plus a
/// couple of hints. This keeps the call a genuinely isolated, tool-less
/// one-shot: it cannot recurse into `ListThreads`/`SummarizeThread` itself
/// and cannot see anything the calling thread hasn't explicitly handed it.
#[derive(Debug, Clone)]
pub struct ThreadSummarizationInput {
    /// The target thread's display title (`title` or `auto_title`), if any.
    pub thread_title: Option<String>,
    /// Optional question or topic supplied by the caller to steer the summary
    /// toward, instead of a general recap.
    pub focus: Option<String>,
    /// The thread's transcript, pre-formatted into readable text by the
    /// calling tool. May represent only part of a very long thread — the
    /// caller is responsible for windowing/truncation and for noting in this
    /// text when that happened.
    pub transcript_text: String,
}

/// Pluggable one-shot summarization back-end used by the `SummarizeThread`
/// engine tool to condense another thread's transcript into prose.
///
/// Defined in this crate so `RunnerContext` can hold an optional engine
/// without creating a circular dependency, mirroring `VerificationEngine`.
///
/// The production implementation (`ProviderThreadSummarizer` in
/// `ao-engine-tools-runner`) makes a single uncached model call through the
/// existing `ProviderClient` seam — no tools, no chat history, just the
/// transcript text in and a prose summary out. Tests inject a scripted mock
/// so no live provider is needed.
#[async_trait]
pub trait ThreadSummarizationEngine: Send + Sync {
    async fn summarize(&self, input: ThreadSummarizationInput) -> Result<String, String>;
}
