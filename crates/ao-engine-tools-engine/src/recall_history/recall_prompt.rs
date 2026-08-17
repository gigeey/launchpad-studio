pub const DESCRIPTION: &str = "Extend your context window backward by returning the N messages immediately \
before the oldest message currently loaded in your context. Use this when you detect a reference \
to earlier conversation content that is no longer in your active window. The tool returns a \
[Recalled context (N messages)] block you can use to resolve the reference. If you are already \
at the beginning of the session history, a structured message indicates there are no earlier messages.\n\n\
Note: RecallHistory always extends backward from the current window's floor — no keyword search \
(deferred to v2). Clamped to a maximum of 100 messages per call.";
