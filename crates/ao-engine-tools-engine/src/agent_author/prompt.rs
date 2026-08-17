use serde_json::{json, Value};

/// How to use AgentAuthor and when.
pub const DESCRIPTION: &str = "\
Creates and edits AgentProfiles — most importantly, an agent's own `persona` \
and `special_instructions`. All operations dispatch through the `op` field.

**Self-editing:** call `update` with your own agent id to change your persona \
or behavior rules. The edit applies live and takes effect on your NEXT turn \
(the composed-context cache is invalidated as part of the same call). The \
previous persona/special_instructions — or the legacy system_prompt blob, if \
neither was set yet — is copied into `legacy_system_prompt` first, so the \
prior behavior is always one more `update` away. There is no confirmation \
step in front of a self-edit, so write the replacement carefully.

**Not for preferences:** a one-off preference, correction, or fact belongs in \
the Memory tool (MemoryWrite), not here — reserve persona/special_instructions \
edits for durable, structural changes to how you behave.

**Ops:**
- `create` — required: `name`, `description`. Optional: `template` \
  ('claude' | 'cursor' | 'codex' | 'antigravity', defaults to 'claude'), \
  `runner_mode` ('cli' | 'api', defaults to 'cli'), `native_provider`, \
  `persona`, `special_instructions`, `model`, `emoji`. Returns the new \
  agent's `id`.
- `update` — required: `id`. Patch semantics: only fields present in the \
  call change; omitted fields keep their current value. Returns the full \
  updated profile.
- `get` — required: `id`. Returns the full profile — read a target's \
  current values before rewriting them.
- `list` — returns `[{id, name, description}]` for every agent.

**Capability gate:** `skills`, `workflows`, and `enabled_plugins` are only \
writable on `update` when `allow_capability_changes: true` is passed \
(default false) — otherwise they are ignored even if present in the call, so \
a persona-only edit can never accidentally strip an agent's tools. When \
allowed, every name in `skills` is checked against the caller's live skill \
registry; an unrecognized name rejects the whole update rather than silently \
writing a dangling reference.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "op": {
                "type": "string",
                "enum": ["create", "update", "get", "list"],
                "description": "Which operation to perform."
            },
            "id": {
                "type": "string",
                "description": "Agent id. Required for 'update' and 'get'. Ignored for 'create' (a fresh id is generated) and 'list'."
            },
            "name": {
                "type": "string",
                "description": "Agent display name. Required for 'create'; when present on 'update', renames the agent."
            },
            "description": {
                "type": "string",
                "description": "One-line description of the agent's purpose. Required for 'create'; optional patch on 'update'."
            },
            "template": {
                "type": "string",
                "enum": ["claude", "cursor", "codex", "antigravity"],
                "description": "'create' only. Which CLI provider preset to scaffold the new agent with. Defaults to 'claude'."
            },
            "runner_mode": {
                "type": "string",
                "enum": ["cli", "api"],
                "description": "'create' only. Locked at creation time. Defaults to 'cli'."
            },
            "native_provider": {
                "type": "string",
                "enum": ["anthropic", "openai", "openrouter"],
                "description": "'create' only. Selects the in-process API provider when runner_mode is 'api'; ignored for 'cli' agents."
            },
            "persona": {
                "type": "string",
                "description": "The agent's identity, voice, and expertise. On 'update', replaces the current value; setting it archives the prior persona/special_instructions to legacy_system_prompt first (one-step undo)."
            },
            "special_instructions": {
                "type": "string",
                "description": "Behavior rules and do's/don'ts. On 'update', replaces the current value; setting it archives the prior persona/special_instructions to legacy_system_prompt first (one-step undo)."
            },
            "model": {
                "type": "string",
                "description": "Model identifier override for the agent's provider."
            },
            "emoji": {
                "type": "string",
                "description": "Sidebar emoji for the agent."
            },
            "allow_capability_changes": {
                "type": "boolean",
                "default": false,
                "description": "'update' only. Must be true to change 'skills', 'workflows', or 'enabled_plugins' — leaving it false (the default) leaves those fields untouched even if present in the call."
            },
            "skills": {
                "type": "array",
                "items": {"type": "string"},
                "description": "'update' only, requires allow_capability_changes: true. Replaces the agent's enabled skill list. Every name is validated against the calling agent's live skill registry; an unknown name rejects the whole call."
            },
            "workflows": {
                "description": "'update' only, requires allow_capability_changes: true. Replaces the agent's workflow binding: the string \"all\", a list of workflow ids, or null to clear it."
            },
            "enabled_plugins": {
                "type": "object",
                "description": "'update' only, requires allow_capability_changes: true. Replaces the agent's per-plugin enablement map, keyed by plugin name to {enabled, enabled_skills}."
            }
        },
        "required": ["op"],
        "additionalProperties": false
    })
}
