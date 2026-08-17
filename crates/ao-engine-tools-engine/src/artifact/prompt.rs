use serde_json::{json, Value};

/// House voice, following `memory/prompt.rs`'s shape: imperative one-liner →
/// params with enum docs → explicit `Returns`.
pub const ARTIFACT_WRITE_DESCRIPTION: &str = "\
Publish a renderable artifact — a typed dataset or a freeform HTML document — so it can be \
displayed inline in the thread, browsed in the Assets panel, and reopened later.

Omit 'id' to publish a brand-new artifact. Pass 'id' to edit an existing artifact in place \
instead of creating a duplicate: its payload is replaced with what you send now (size and \
checksum are recomputed and 'updated_at' is bumped), while its title, refresh_intent, pin \
state, and group membership are left exactly as they were. 'renderer' must still match the \
renderer the artifact was originally published with — an update cannot reformat an artifact \
into a different view, so publish a new one instead if you need that. If 'id' doesn't match \
an existing artifact, the call fails with an error rather than quietly creating a new one — \
fix the id, or drop it to create fresh.

'renderer' picks which view draws the payload:
- 'list' — a linear list of items
- 'cards' — a grid of cards
- 'table' — rows and columns
- 'board' — a kanban-style board of columns
- 'metric' — one or a few headline numbers
- 'chart' — a chart (bar/line/pie/etc., shape carried in payload)
- 'html' — freeform sandboxed HTML you author directly

'payload' must match 'renderer': a JSON object for the typed renderers (list/cards/table/board/ \
metric/chart), or an HTML string for 'html'. For interactive 'html', embed the WHOLE dataset the \
interaction needs directly in the payload and reveal/reshape it client-side with your own script \
— do not author the page assuming it can fetch more data per click. There is no live bridge back \
to you yet, so any click handler that assumes one will render inert. Over-fetching once at write \
time is the correct default, not a shortcut.

**Interactive HTML artifacts run inside a locked-down, opaque-origin sandbox.** The iframe \
is sandboxed with `allow-scripts` but WITHOUT `allow-same-origin` — a permanent security \
invariant, not something that can be relaxed. That puts the page at a null/opaque origin, \
where any direct read or write of `localStorage`, `sessionStorage`, or `document.cookie` \
throws a `SecurityError` and aborts your entire script. If that access runs early (e.g. \
reading saved state before the first render), NOTHING renders — the page freezes at its \
placeholder markup (a blank-looking UI with zeroed stats) even though the HTML and CSS are \
correct.

Rules for any interactive artifact you generate:
- NEVER touch `localStorage`, `sessionStorage`, or `document.cookie` directly. Route every \
storage read/write through a try/catch in-memory shim, e.g.:

```js
const _mem = {};
const store = {
  get(k){ try { return localStorage.getItem(k); } catch { return k in _mem ? _mem[k] : null; } },
  set(k,v){ try { localStorage.setItem(k,v); } catch { _mem[k] = v; } },
};
```

Cross-reload persistence isn't available in an opaque-origin frame anyway, so the in-memory \
fallback loses nothing the sandbox could have offered — but it guarantees a single storage \
call can never blank the page.
- Guard any other origin-sensitive API the same way (`indexedDB`, `BroadcastChannel`) so one \
`SecurityError` can't abort rendering.

To link out to an external page, use a normal anchor — `<a href=\"https://...\" \
target=\"_blank\" rel=\"noopener noreferrer\">` — which opens in a new browser tab. Do NOT \
try to navigate via `window.location`, `window.top`, or any top-frame navigation; the sandbox \
blocks it and the click will silently appear to do nothing. `window.open(url)` only succeeds \
when called directly inside a user-gesture handler (e.g. an `onclick`), never on page load. \
Keep the artifact's own content self-contained regardless — include a short summary inline — \
so it still delivers value even if a reader never follows the link.

Exact payload shape per typed renderer (a wrong shape is rejected with an error telling you how \
to fix it — fix and retry rather than resubmitting the same shape):
- 'list': { \"items\": [...] } — each item is a string or { title, subtitle?, description? }.
- 'cards': { \"items\": [...] } — each item is an object with title/subtitle/description/image \
(subtitle also accepts 'tag' or 'category'; description also accepts 'text', 'body', 'summary').
- 'table': { \"columns\": [...], \"rows\": [...] } — 'columns' MUST be plain strings (column \
headers), never { key, label } objects. 'rows' is an array of either arrays (values in the same \
order as 'columns') or objects keyed by the exact column strings. \
Example: { \"columns\": [\"Task\", \"Owner\"], \"rows\": [[\"Write docs\", \"Alex\"]] }.
- 'board': { \"columns\": [...] } — each column is { title, items: [...] }, each item an object \
with at least 'title'.
- 'metric': { \"metrics\": [...] } of { label, value, delta? }, or a single flat { label, value, \
delta? } object.
- 'chart': { \"labels\": [...], \"series\": [...] } — 'series' items are { name?, values: [...] }, \
one number per label.

'refresh_intent' controls whether this artifact can be regenerated later:
- 'none' (default) — a one-shot snapshot; it never updates in place.
- 'whole_artifact' — the whole payload can be regenerated on request. Requires 'refresh_prompt': \
a self-contained instruction (with no dependency on this conversation's context) that fully \
describes how to reproduce the payload, since it will be replayed later on its own.

'capabilities' declares an allowlist of data slices this artifact may request from a future \
in-artifact fetch bridge — e.g. [{ slice: 'email.body', params_schema: {...} }]. Optional, \
defaults to []. That bridge does not exist yet in this build, so a declared capability is stored \
for forward-compatibility only and is not served — do not rely on it to satisfy an interaction; \
use the over-fetch convention above instead.

'intent_note' is an optional ONE-LINE summary of what THIS write is for, scoped to this artifact \
only — if the user's message asked for several things, strip everything that isn't about this \
artifact. It describes this specific write, not the conversation as a whole: on create, say what \
the artifact is for ('a table of Q3 launch tasks by owner'); on an edit-in-place, say what \
changed ('added the Q4 rows the user asked for'), not a running history of every edit so far. \
Each write's note is recorded on the artifact's own history, so there's no need to repeat context \
from earlier writes.

Returns { id, renderer, refresh_intent, title } on success — id is this artifact's persisted \
identifier, useful if you need to reference it again.";

pub fn artifact_write_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Id of an existing artifact to update in place. Omit to publish a new artifact. If set but no artifact with that id exists, the call errors instead of creating one."
            },
            "title": {
                "type": "string",
                "description": "Card and window title for this artifact."
            },
            "renderer": {
                "type": "string",
                "enum": ["list", "cards", "table", "board", "metric", "chart", "html"],
                "description": "Which view draws the payload. See tool description for the full list."
            },
            "payload": {
                "type": ["object", "string"],
                "description": "A JSON object for typed renderers (list/cards/table/board/metric/chart), or an HTML string for renderer='html'."
            },
            "refresh_intent": {
                "type": "string",
                "enum": ["none", "whole_artifact"],
                "default": "none",
                "description": "'none' (default): a one-shot snapshot. 'whole_artifact': the payload can be regenerated later by replaying refresh_prompt."
            },
            "refresh_prompt": {
                "type": "string",
                "description": "Required when refresh_intent='whole_artifact'. A self-contained instruction for regenerating the payload from scratch."
            },
            "capabilities": {
                "type": "array",
                "default": [],
                "description": "Optional allowlist of data slices this artifact may request from the (not-yet-built) in-artifact fetch bridge. Stored now, served in a future release.",
                "items": {
                    "type": "object",
                    "properties": {
                        "slice": { "type": "string" },
                        "params_schema": { "type": "object" }
                    },
                    "required": ["slice", "params_schema"],
                    "additionalProperties": false
                }
            },
            "intent_note": {
                "type": "string",
                "description": "Optional one-line, artifact-scoped summary of what this specific write is for (strip anything from the user's request that isn't about this artifact). Point-in-time for this write only, not a cumulative summary — on create, what the artifact is for; on an edit, what changed. Recorded on the artifact's own history."
            }
        },
        "required": ["title", "renderer", "payload"],
        "additionalProperties": false
    })
}
