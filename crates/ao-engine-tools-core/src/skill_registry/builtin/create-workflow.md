---
name: create-workflow
description: Teaches how to author a reusable Launchpad Studio Workflow — a folder on disk (workflow.yaml + phase assets) that runs as a multi-phase task in the app's Tasks sidebar. Use when asked to create, save, author, or define a new workflow/task template. Not for a one-off script, and not for a Skill (SKILL.md), which is a separate system handled by SkillRegister.
when-to-use: Before writing any workflow.yaml, or whenever a repeatable multi-phase agent/script/user process should become a nameable, reusable task template.
context: inline
---
# Authoring a Launchpad Studio Workflow

A **workflow** here is a folder on disk, not a script. It lives at
`<data_dir>/workflows/<id>/` (the data dir defaults to `~/.launchpad_studio`,
or `$LAUNCHPAD_STUDIO_DATA_DIR` if set) and contains a `workflow.yaml`
definition plus one asset folder per phase. Once saved, it shows up in the
Tasks sidebar and can be run repeatedly as a **task** — each run is a
separate instance with its own output directory. This is unrelated to
`SkillRegister` (which authors a `SKILL.md` instruction file) — a workflow
is a multi-phase task template, not a single skill invocation.

## Directory layout

```
<data_dir>/workflows/
└── my-workflow/                    # Directory name = the workflow's `id`
    ├── workflow.yaml               # Required: workflow definition
    ├── interview/
    │   └── prompt.md               # Prompt phase: markdown instructions for the agent
    ├── generate-prd/
    │   ├── prompt.md
    │   └── schema.json              # Optional: JSON schema for structured output
    └── build/
        └── run.sh                  # Folder phase: executable script, full shell access
```

Input and pause phases don't need a directory or files — see below.

## workflow.yaml

```yaml
id: "my-workflow"                   # Required. Must match the directory name.
name: "My Workflow"                 # Required. Human-readable.
version: "1.0.0"                    # Optional.
description: "What this workflow does"  # Optional. Shown in UI.

phases:
  - id: "phase-id"                  # Required. Unique within this workflow.
    name: "Phase Name"              # Required.
    intent: "What this phase does"  # Optional. Guides the agent's approach.
    path: "phase-id/prompt.md"      # Required. Relative to the workflow dir.
    phase_type: "prompt"            # prompt | folder | input | pause
    auto_advance: true              # Default true; false pauses for review after this phase.
    schema: "phase-id/schema.json"  # Optional. JSON schema for structured output.
    inputs: []                      # References to prior phases' outputs.
    outputs: []                     # Declared output artifacts.
    fields: []                      # Form fields — input phases only.
```

`phase_type` auto-detects from the filesystem when omitted: a directory
path → `folder`, a file path → `prompt`. **`input` and `pause` are never
auto-detected — always set them explicitly.**

## The four phase types

- **`prompt`** — an agent executes instructions from `path/prompt.md`. The
  engine resolves `{{placeholders}}`, injects prior-phase inputs, intent,
  and (if `schema` is set) the output schema into the agent's context
  before sending the prompt — don't repeat that context in the prompt
  itself; focus it on *how* to do the work.
- **`folder`** — a self-contained `run.sh` at `path/run.sh`, run as
  `bash run.sh` with full shell access (no permission prompts). Use for
  builds, git, deploys, API calls. The engine passes env vars:
  `WORKFLOW_TASK_ID`, `WORKFLOW_PHASE_ID`, `WORKFLOW_OUTPUT_DIR`,
  `WORKFLOW_STATUS_FILE`, `WORKFLOW_WORKING_DIR`, and one
  `WORKFLOW_INPUT_<ID>` (uppercased) per declared input. The script should
  write declared outputs into `$WORKFLOW_OUTPUT_DIR` and may report
  progress by writing JSON like
  `{"status":"running","message":"...","percent":50}` to
  `$WORKFLOW_STATUS_FILE`. Exit 0 + all declared outputs present →
  completed; nonzero exit or a missing declared output → failed.
- **`input`** — renders a form in the UI from `fields: [{name, label,
  placeholder?, description?, required?}]`. Submitted values are written to
  the task's shared `outputs/inputs.yaml` and become available as
  `{{name}}` placeholders in every downstream prompt phase.
- **`pause`** — halts the task until the user manually resumes; use as a
  review gate (pair with `auto_advance: false`).

## Wiring phases together

- **Outputs**: `outputs: [{id, filename?, description?}]` on the producing
  phase. `filename` defaults to `{id}.txt`; all outputs land flat in the
  task's `output/` directory (no nesting).
- **Inputs**: `inputs: [{id, from_phase, from_output}]` on the consuming
  phase — `from_phase`/`from_output` must name a real phase id and its
  output id declared earlier in the file. The engine reads that output
  file and injects its contents into context under a heading like
  `### <id> (from phase: <from_phase>)`. Before a phase starts, the engine
  verifies every declared input file exists; if one is missing, the phase
  pauses instead of running.
- **Placeholders**: `{{key}}` in any `prompt.md`, resolved at runtime from
  (in priority order) task-creation context, then `inputs.yaml` values. An
  unmatched placeholder is left as literal `{{key}}` text.

## Steps to follow when asked to create a workflow

1. Sketch the phase sequence first: what's agent work (`prompt`), what's
   deterministic (`folder`), where user input or a review gate belongs
   (`input`/`pause`). Decide phase order so every phase's `inputs` come
   from phases that already ran.
2. Pick a kebab-case `id` — it must equal the directory name.
3. Use `Write` to create `<data_dir>/workflows/<id>/workflow.yaml`, then
   each phase's asset file(s) (`prompt.md`, `schema.json`, `run.sh`) at the
   paths the YAML declares. Give `run.sh` scripts `set -euo pipefail` and
   make them executable.
4. Sanity-check before telling the user it's done:
   - the folder name equals `id`
   - every phase id is unique; every `from_phase`/`from_output` reference
     resolves to a real, earlier phase/output
   - `input`/`pause` phases declare `phase_type` explicitly
   - output `filename`s are unique across the whole workflow (flat output dir)
   - any `schema.json` is valid JSON Schema
5. Tell the user where it's saved and that it now appears in the Tasks
   sidebar (may need a workflow list refresh) — creating a task from it is
   how they run it.
