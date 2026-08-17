use serde_json::{json, Value};

pub const DESCRIPTION: &str = "Invoke a named Studio skill (registered under \
    `<data_dir>/skills/` or via an enabled plugin pool). Provide `skill` \
    (required, the skill name) and optionally `args` (a free-form string \
    passed as $ARGUMENTS inside the skill body). Inline skills inject their \
    body as a follow-up user message; fork skills spawn a synchronous child \
    runner.\n\n\
    Note: this tool dispatches Studio skills only. The host CLI process \
    that embeds Studio may expose its own separate skill system under a \
    different tool; those skills are invoked through that tool, not through \
    `RunSkill`. The two systems coexist — Studio skills listed in the \
    # Studio Skills system-reminder block are the ones `RunSkill` knows about.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "skill": {
                "type": "string",
                "description": "Name of the skill to invoke."
            },
            "args": {
                "type": "string",
                "description": "Optional free-form arguments passed as $ARGUMENTS inside the skill body."
            }
        },
        "required": ["skill"],
        "additionalProperties": false
    })
}

pub const DESCRIPTION_SKILL_WRITE: &str = "Create or update a Studio \
    user-pool skill (writes to `<data_dir>/skills/<name>/SKILL.md` and \
    registers it on the calling agent's profile). Provide `name` (required, \
    [a-z0-9_-], max 64 chars), `description` (required, 1-240 chars), \
    `body` (required, full SKILL.md content including frontmatter), and \
    optionally `override` (boolean, default false). Returns an error if \
    the skill already exists and override is false.\n\n\
    `body` format — YAML frontmatter delimited by `---` lines at the very \
    start, then the skill instructions as Markdown:\n\
    ```\n\
    ---\n\
    name: my-skill              # required; must equal the `name` argument\n\
    description: What it does   # required, 1-240 chars\n\
    when-to-use: Triggers here  # optional, helps a model pick this skill\n\
    context: inline             # optional: `inline` (default) injects the \
    body as a follow-up message; `fork` runs it in a child agent\n\
    allowed-tools: [Read, Grep] # optional; omit to allow all tools\n\
    arguments:                  # optional; `$ARGUMENTS` interpolates the \
    caller's args string\n\
      - name: target\n\
        required: true\n\
    ---\n\
    Instructions for the agent go here.\n\
    ```\n\
    Do not write `origin`, `version`, `retired`, `retired-reason`, \
    `superseded-by`, or `distilled-from` — those are owned by the skill \
    lifecycle and are stamped automatically. Unknown keys are ignored. \
    Frontmatter that fails to parse produces a skill that can never load, so \
    keep it valid YAML.\n\n\
    Trust gate: every newly written or updated skill is staged for review and \
    is NOT invokable via `RunSkill` until a human approves it — there is no \
    exemption for read-only or otherwise low-stakes skills. Any \
    `disable-model-invocation` value written in `body` is overridden by the \
    gate. Approval happens in Studio under Memories → Skills held for review; \
    tell the user that is where to go, because nothing you can call will \
    approve it for them.\n\n\
    Studio-only: this tool does not touch any skill system owned by the \
    host CLI process that embeds Studio — it writes to the Studio user pool \
    only.";

pub fn skill_write_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Skill name: [a-z0-9_-], max 64 chars."
            },
            "description": {
                "type": "string",
                "description": "Short description, 1-240 chars."
            },
            "body": {
                "type": "string",
                "description": "Full SKILL.md content including YAML frontmatter."
            },
            "override": {
                "type": "boolean",
                "description": "If true, overwrite existing user-pool skills. Default false.",
                "default": false
            }
        },
        "required": ["name", "description", "body"],
        "additionalProperties": false
    })
}
