use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Run an independent verification pass to decide whether the project goal has \
been genuinely met.

Two modes are available:

mode='quick' (default): Judges the goal against the tasklist completion \
summaries via a single model call. Fast and useful for mid-flight gap checks, \
but it cannot confirm that code actually compiles, tests pass, or deliverables \
exist — it only reads claims in summaries. Use this between work batches to \
stay on track.

mode='full': Spawns an isolated, read-only inspection subagent that opens the \
project's working directory, reads source files, inspects git diffs, and runs \
the test suite (if discoverable). Verifies claims against real artifacts rather \
than summaries. This mode is REQUIRED before calling ProjectComplete — the \
completion gate will reject a project that only has a quick-mode pass.

Workflow:
1. Use mode='quick' after each work batch to track gaps cheaply.
2. Before attempting ProjectComplete, run mode='full'.
3. Feed the returned 'gaps' list into follow-up TodoCreate calls for remaining work.

Parameters:
- mode (optional): 'quick' (default, fast summary judge) or 'full' \
  (inspection subagent, required for completion).
- extra_evidence (optional): additional context for the verifier — file paths, \
  test output snippets, or artifact references not captured in tasklist summaries.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "mode": {
                "type": "string",
                "enum": ["quick", "full"],
                "description": "Verification engine to use. 'quick' (default) makes a single model call over tasklist summaries — fast and cheap, good for mid-flight gap checks. 'full' spawns an isolated read-only inspector that opens the working directory, reads files, and runs tests — required before calling ProjectComplete."
            },
            "extra_evidence": {
                "type": "string",
                "description": "Optional additional evidence for the verifier: file paths, test output, artifact descriptions, etc."
            }
        },
        "additionalProperties": false
    })
}
