use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::RwLock;

use ao_persistence::workflow_store::{TaskStore, WorkflowStore};
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::workflow::{PhaseDefinition, PhaseState, PhaseStatus, TaskSnapshot, TaskStatus};

use crate::event_bus::EventBus;
use crate::workflow_queue_manager::{WfQueueMsg, WorkflowQueueHandle};
use crate::workflow_registry::WorkflowRegistry;

/// Counts regular, non-hidden files in `dir`. Returns 0 if directory cannot be read.
async fn count_output_dir_files(dir: &Path) -> usize {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if let Ok(ft) = entry.file_type().await {
            if ft.is_file() {
                count += 1;
            }
        }
    }
    count
}

/// Lists regular, non-hidden files in `dir`, sorted, truncated to 20.
/// Returns "Found in output dir: [a.md, b.md]" or "(empty)".
/// Returns an empty string if the directory cannot be read.
async fn list_output_dir_files(dir: &Path) -> String {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return String::new(),
    };
    let mut names: Vec<String> = Vec::new();
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if let Ok(ft) = entry.file_type().await {
            if ft.is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    let total = names.len();
    let display: Vec<&String> = names.iter().take(20).collect();
    let list_str = if display.is_empty() {
        "(empty)".to_string()
    } else {
        let mut s = display.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ");
        if total > 20 {
            s.push_str(&format!(" ... (+{} more)", total - 20));
        }
        s
    };
    format!(" Found in output dir: {}", list_str)
}

/// Deterministic conductor that creates tasks and builds phase context
/// for sequential workflow execution. Does NOT call agents directly —
/// it builds context that gets queued to agents via the QueueManager.
pub struct WorkflowRunner {
    workflow_registry: Arc<RwLock<WorkflowRegistry>>,
    task_store: TaskStore,
    workflow_store: WorkflowStore,
    event_bus: Arc<EventBus>,
    /// Optional handle for sending lifecycle messages back to the workflow
    /// queue manager. Wired post-construction via [`set_workflow_queue`]
    /// because the queue manager itself depends on `Arc<WorkflowRunner>` —
    /// the chicken-and-egg circle is broken by installing the handle later.
    /// `None` is fine in unit tests that drive the runner directly.
    workflow_queue: RwLock<Option<WorkflowQueueHandle>>,
}

impl WorkflowRunner {
    pub fn new(
        workflow_registry: Arc<RwLock<WorkflowRegistry>>,
        task_store: TaskStore,
        workflow_store: WorkflowStore,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            workflow_registry,
            task_store,
            workflow_store,
            event_bus,
            workflow_queue: RwLock::new(None),
        }
    }

    /// Install the workflow queue handle so the runner can notify the
    /// queue manager about phase lifecycle transitions. Idempotent —
    /// re-installs the handle, replacing any prior value.
    pub async fn set_workflow_queue(&self, handle: WorkflowQueueHandle) {
        *self.workflow_queue.write().await = Some(handle);
    }

    /// Access the workflow registry.
    pub fn workflow_registry(&self) -> &Arc<RwLock<WorkflowRegistry>> {
        &self.workflow_registry
    }

    /// Create a new workflow task. Returns the task_id.
    pub async fn create_task(
        &self,
        workflow_id: &str,
        project_name: &str,
        working_directory: Option<String>,
        context: Option<String>,
    ) -> Result<String, AoError> {
        // Resolve workflow from registry
        let registry = self.workflow_registry.read().await;
        let definition = registry.get_definition(workflow_id).ok_or_else(|| {
            AoError::Internal(format!("Workflow '{}' not found in registry", workflow_id))
        })?;

        // Generate task ID: {workflow_id}_{timestamp}_{short_uuid}
        let timestamp = Utc::now().format("%Y%m%d").to_string();
        let short_uuid = &uuid::Uuid::new_v4().to_string()[..5];
        let task_id = format!("{}_{}{}", workflow_id, timestamp, short_uuid);

        // Build context map
        let mut context_map = std::collections::HashMap::new();
        if let Some(ctx) = context {
            context_map.insert("user_context".to_string(), ctx);
        }

        // Create initial snapshot
        let snapshot = TaskSnapshot {
            status: TaskStatus::Pending,
            workflow: workflow_id.to_string(),
            workflow_version: definition.version.clone(),
            created: Utc::now(),
            project_name: project_name.to_string(),
            working_directory,
            context: context_map,
            phases: std::collections::HashMap::new(),
        };

        // Drop the registry lock before doing I/O
        let wf_name = definition.name.clone();
        drop(registry);

        // Create task directory and write snapshot
        self.task_store.create_task_dir(&task_id).await?;
        self.task_store
            .write_task_snapshot(&task_id, &snapshot)
            .await?;

        // Emit WorkflowTaskCreated event
        self.event_bus
            .emit(
                &task_id,
                &format!("workflow:{}", workflow_id),
                None,
                AgentEventPayload::WorkflowTaskCreated {
                    task_id: task_id.clone(),
                    workflow_id: workflow_id.to_string(),
                    project_name: project_name.to_string(),
                },
            )
            .await;

        tracing::info!(
            task_id = %task_id,
            workflow = %workflow_id,
            workflow_name = %wf_name,
            project = %project_name,
            "Created workflow task"
        );

        Ok(task_id)
    }

    /// Build a summary of a newly created task for the chat agent.
    /// Includes task directory, phase list with expected outputs, and file paths
    /// so the agent can read phase prompts/schemas on demand when pre-filling.
    pub async fn build_create_summary(
        &self,
        task_id: &str,
        workflow_id: &str,
    ) -> Result<String, AoError> {
        let registry = self.workflow_registry.read().await;
        let definition = registry.get_definition(workflow_id).ok_or_else(|| {
            AoError::Internal(format!("Workflow '{}' not found in registry", workflow_id))
        })?;

        let task_dir = self.task_store.base_path().join(task_id);
        let output_dir = format!("{}/output", task_dir.display());
        let workflows_dir = self.workflow_store.base_path();
        let workflow_dir = workflows_dir.join(workflow_id);

        let mut sections = Vec::new();
        sections.push(format!(
            "## Task Created\n\
             - **Task ID**: `{}`\n\
             - **Workflow**: {} (`{}`)\n\
             - **Output directory**: `{}`\n\
             - **Status**: Pending",
            task_id, definition.name, workflow_id, output_dir
        ));

        sections.push(format!(
            "## Workflow Directory\n\
             `{}`\n\n\
             All workflow files (phase prompts, schemas, `workflow.yaml`, sibling \
             resources, and nested folder-phase directories) live under this path. \
             Use it directly — do NOT run `find` or otherwise search the filesystem \
             to locate workflow files.",
            workflow_dir.display()
        ));

        sections.push(format!(
            "## Pre-filling Phases\n\
             Review the conversation above. For any phase where you already have \
             sufficient context, you can pre-fill its output:\n\n\
             1. **Read the phase prompt** to understand the expected output format. \
             Phase prompts are at `{workflows_dir}/{workflow_id}/<phase.path>`.\n\
             2. If a phase has a **schema**, read it too (path listed below).\n\
             3. Write the output using `write_output`, then mark it complete with `complete_phase`.\n\
             4. Only pre-fill phases you have **complete** information for.\n\n\
             **Watch your output size.** Each `write_output` call counts against your \
             response token budget. If a phase's output is large (multi-KB JSON, long \
             markdown), pre-filling several phases in one response can hit the response \
             cap and silently truncate later writes. When outputs are heavy, prefer \
             fewer phases per response so each `write_output` lands cleanly.\n\n\
             When done pre-filling (or if no phases can be pre-filled), \
             ask the user if they are ready to start the task.\n\n\
             **IMPORTANT**: You MUST ask the user whether to start the task. Do NOT \
             start it automatically. Say something like \"Task is ready. Shall I start it?\"",
            workflows_dir = workflows_dir.display(),
            workflow_id = workflow_id
        ));

        // Clone data we need before dropping registry lock
        let phases = definition.phases.clone();
        drop(registry);

        sections.push(String::from("## Phases"));

        for (i, phase) in phases.iter().enumerate() {
            let mut phase_block = format!(
                "### Phase {}: {} (`{}`)",
                i + 1,
                phase.name,
                phase.id
            );

            // Phase type
            let phase_type_str = match phase.phase_type {
                Some(ao_protocol::workflow::PhaseType::Prompt) => "prompt",
                Some(ao_protocol::workflow::PhaseType::Folder) => "folder",
                Some(ao_protocol::workflow::PhaseType::Input) => "input",
                Some(ao_protocol::workflow::PhaseType::Pause) => "pause",
                None => "auto-detected",
            };
            phase_block.push_str(&format!("\n- **Type**: {}", phase_type_str));

            if let Some(ref intent) = phase.intent {
                phase_block.push_str(&format!("\n- **Intent**: {}", intent));
            }

            // Phase prompt path
            if !phase.path.is_empty() {
                phase_block.push_str(&format!(
                    "\n- **Prompt**: `{}/{}/{}`",
                    workflows_dir.display(), workflow_id, phase.path
                ));
            }

            // Schema path
            if let Some(ref schema_path) = phase.schema {
                phase_block.push_str(&format!(
                    "\n- **Schema**: `{}/{}/{}`",
                    workflows_dir.display(), workflow_id, schema_path
                ));
            }

            // Expected outputs
            if !phase.outputs.is_empty() {
                phase_block.push_str("\n- **Expected outputs**:");
                for output in &phase.outputs {
                    let default_filename = format!("{}.txt", output.id);
                    let filename = output.filename.as_deref().unwrap_or(&default_filename);
                    phase_block.push_str(&format!("\n  - `{}`: {}",
                        filename,
                        output.description.as_deref().unwrap_or("")));
                }
            }

            // Input phase fields
            if !phase.fields.is_empty() {
                phase_block.push_str("\n- **Input fields** (write to `inputs.yaml`):");
                for field in &phase.fields {
                    let req = if field.required { " (required)" } else { " (optional)" };
                    phase_block.push_str(&format!("\n  - `{}`: {}{}", field.name, field.label, req));
                }
            }

            // Inputs from other phases
            if !phase.inputs.is_empty() {
                phase_block.push_str("\n- **Depends on**:");
                for input in &phase.inputs {
                    if let (Some(ref fp), Some(ref fo)) = (&input.from_phase, &input.from_output) {
                        phase_block.push_str(&format!("\n  - `{}` from phase `{}`", fo, fp));
                    }
                }
            }

            sections.push(phase_block);
        }

        Ok(sections.join("\n\n"))
    }

    /// Get the next phase to execute for a task.
    /// Returns None if all phases are done.
    pub async fn get_next_phase(
        &self,
        task_id: &str,
    ) -> Result<Option<PhaseDefinition>, AoError> {
        let snapshot = self.task_store.read_task_snapshot(task_id).await?;
        let registry = self.workflow_registry.read().await;
        let definition =
            registry
                .get_definition(&snapshot.workflow)
                .ok_or_else(|| {
                    AoError::Internal(format!(
                        "Workflow '{}' not found in registry",
                        snapshot.workflow
                    ))
                })?;

        // Find the first phase whose id is not in the snapshot's phases map,
        // or whose status is Paused (eligible for re-evaluation)
        for phase in &definition.phases {
            if !snapshot.phases.contains_key(&phase.id) {
                return Ok(Some(phase.clone()));
            }
            if let Some(state) = snapshot.phases.get(&phase.id) {
                if matches!(state.status, PhaseStatus::Paused) {
                    return Ok(Some(phase.clone()));
                }
            }
        }

        Ok(None)
    }

    /// Resolve the actual output filename for a phase output reference.
    /// Looks up the PhaseDefinition by `from_phase` in the workflow definition,
    /// finds the PhaseOutput with matching `output_id`, and returns its filename.
    /// Falls back to `{output_id}.txt` if no filename is set or phase/output not found.
    pub async fn resolve_output_filename(
        &self,
        workflow_id: &str,
        from_phase: &str,
        output_id: &str,
    ) -> String {
        let fallback = format!("{}.txt", output_id);
        let registry = self.workflow_registry.read().await;
        let definition = match registry.get_definition(workflow_id) {
            Some(def) => def,
            None => return fallback,
        };
        let phase_def = match definition.phases.iter().find(|p| p.id == from_phase) {
            Some(p) => p,
            None => return fallback,
        };
        // Input-type phases write to the shared inputs.yaml file
        if phase_def.phase_type == Some(ao_protocol::workflow::PhaseType::Input) {
            return "inputs.yaml".to_string();
        }
        phase_def
            .outputs
            .iter()
            .find(|o| o.id == output_id)
            .and_then(|o| o.filename.clone())
            .unwrap_or(fallback)
    }

    /// Build the context block for a phase, including prompt, inputs, schema, and guidance.
    pub async fn build_phase_context(
        &self,
        task_id: &str,
        phase: &PhaseDefinition,
    ) -> Result<String, AoError> {
        let snapshot = self.task_store.read_task_snapshot(task_id).await?;
        let registry = self.workflow_registry.read().await;
        let definition =
            registry
                .get_definition(&snapshot.workflow)
                .ok_or_else(|| {
                    AoError::Internal(format!(
                        "Workflow '{}' not found in registry",
                        snapshot.workflow
                    ))
                })?;

        let total_phases = definition.phases.len();
        let phase_index = definition
            .phases
            .iter()
            .position(|p| p.id == phase.id)
            .unwrap_or(0);

        let workflow_id = definition.id.clone();
        let workflow_name = definition.name.clone();
        let workflow_dir = self.workflow_store.base_path().join(&workflow_id);

        // Pre-resolve output filenames for inputs while we hold the registry lock
        let resolved_input_filenames: Vec<Option<String>> = phase
            .inputs
            .iter()
            .map(|input| {
                match (&input.from_phase, &input.from_output) {
                    (Some(from_phase), Some(from_output)) => {
                        let filename = definition
                            .phases
                            .iter()
                            .find(|p| &p.id == from_phase)
                            .and_then(|p| {
                                p.outputs
                                    .iter()
                                    .find(|o| &o.id == from_output)
                                    .and_then(|o| o.filename.clone())
                            })
                            .unwrap_or_else(|| format!("{}.txt", from_output));
                        Some(filename)
                    }
                    _ => None,
                }
            })
            .collect();

        drop(registry);

        let mut sections = Vec::new();

        // Header
        sections.push(format!(
            "# Workflow: {}\n## Task: {}\n## Phase {} of {}: {}",
            workflow_name,
            snapshot.project_name,
            phase_index + 1,
            total_phases,
            phase.name,
        ));

        // Phase intent
        if let Some(ref intent) = phase.intent {
            sections.push(format!("## Intent\n{}", intent));
        }

        // Input file contents from prior phase outputs
        if !phase.inputs.is_empty() {
            let mut input_section = String::from("## Inputs");
            for (input, resolved_filename) in
                phase.inputs.iter().zip(resolved_input_filenames.iter())
            {
                if let (Some(from_phase), Some(filename)) =
                    (&input.from_phase, resolved_filename)
                {
                    match self.task_store.read_output(task_id, filename).await {
                        Ok(content) => {
                            input_section.push_str(&format!(
                                "\n\n### {} (from phase: {})\n```\n{}\n```",
                                input.id, from_phase, content
                            ));
                        }
                        Err(_) => {
                            input_section.push_str(&format!(
                                "\n\n### {} (from phase: {})\n*Not yet available*",
                                input.id, from_phase
                            ));
                        }
                    }
                }
            }
            sections.push(input_section);
        }

        // Output path and schema
        if !phase.outputs.is_empty() {
            let mut output_section = String::from("## Expected Outputs");
            for output in &phase.outputs {
                let default_filename = format!("{}.txt", output.id);
                let filename = output.filename.as_deref().unwrap_or(&default_filename);
                output_section
                    .push_str(&format!("\n- **{}**: write to `{}`", output.id, filename));
                if let Some(ref desc) = output.description {
                    output_section.push_str(&format!(" — {}", desc));
                }
            }
            sections.push(output_section);
        }

        // Schema if present
        if let Some(ref schema_path) = phase.schema {
            match self
                .workflow_store
                .read_phase_schema(&workflow_id, schema_path)
                .await
            {
                Ok(schema) => {
                    let abs_schema = workflow_dir.join(schema_path);
                    sections.push(format!(
                        "## Output Schema\nSource: `{}`\n\n```json\n{}\n```",
                        abs_schema.display(),
                        schema
                    ));
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        phase_id = %phase.id,
                        "Failed to read phase schema: {}",
                        e
                    );
                }
            }
        }

        // Collect input-phase values for placeholder resolution.
        // Input phases write YAML files to output/{phase_id}.yaml with key-value pairs.
        let mut placeholders: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // Also include task context as placeholders
        for (k, v) in &snapshot.context {
            placeholders.insert(k.clone(), v.clone());
        }

        // Load shared inputs.yaml (all input phases write to this single file)
        if let Ok(content) = self.task_store.read_output(task_id, "inputs.yaml").await {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once(':') {
                    let k = k.trim().to_string();
                    let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                    if !k.is_empty() {
                        placeholders.insert(k, v);
                    }
                }
            }
        }

        // Phase prompt content (with placeholder resolution)
        if !phase.path.is_empty() {
            match self
                .workflow_store
                .read_phase_prompt(&workflow_id, &phase.path)
                .await
            {
                Ok(mut prompt) => {
                    // Resolve {{placeholder}} patterns
                    for (k, v) in &placeholders {
                        prompt = prompt.replace(&format!("{{{{{}}}}}", k), v);
                    }
                    let abs_prompt = workflow_dir.join(&phase.path);
                    sections.push(format!(
                        "## Instructions\nSource: `{}`\n\n{}",
                        abs_prompt.display(),
                        prompt
                    ));
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        phase_id = %phase.id,
                        "Failed to read phase prompt: {}",
                        e
                    );
                }
            }
        }

        // User context
        if let Some(ctx) = snapshot.context.get("user_context") {
            sections.push(format!("## Project Context\n{}", ctx));
        }

        // Workflow directory (contains phase prompts, schemas, run.sh, nested phase folders)
        sections.push(format!(
            "## Workflow Directory\n`{}`\n\nPhase prompts, schemas, and any nested phase \
             files live under this directory. If you need to inspect or reference sibling \
             files, use this path directly instead of searching the filesystem.",
            workflow_dir.display()
        ));

        // Working directory
        if let Some(ref wd) = snapshot.working_directory {
            sections.push(format!("## Working Directory\n`{}`", wd));
        }

        // Task directory (for reading/writing outputs)
        let task_dir = self.task_store.base_path().join(task_id);
        sections.push(format!(
            "## Task Directory\n`{}`\n\nOutput files are written to `{}/output/`. \
             You may read or modify files here directly.",
            task_dir.display(),
            task_dir.display(),
        ));

        // Prefill guidance with concrete tag examples
        let output_dir = format!("{}/output", task_dir.display());

        let mut guidance = String::from(
            "## Guidance\n\
             Before starting work on this phase, check the conversation context above. \
             If prior messages contain relevant work or partial results for this phase, \
             build on that work rather than starting from scratch.\n\n\
             ### Writing outputs\n\
             For each expected output, create the initial version using a `<workflow_task>` tag:",
        );
        for output in &phase.outputs {
            let default_filename = format!("{}.txt", output.id);
            let filename = output.filename.as_deref().unwrap_or(&default_filename);
            guidance.push_str(&format!(
                "\n```xml\n<workflow_task action=\"write_output\" task_id=\"{}\" phase=\"{}\" output=\"{}\">\n... content ...\n</workflow_task>\n```",
                task_id, phase.id, filename
            ));
        }
        guidance.push_str(&format!(
            "\n\n### Editing outputs\n\
             If you need to revise or refine an output after the initial write (e.g., the user \
             asks for changes), edit the file directly at `{output_dir}/` using your file editing \
             tools rather than re-emitting the entire content via `write_output`. This is more \
             efficient for iterative refinement.\n\n\
             ### Completing the phase\n\
             After ALL outputs have been written, mark the phase complete:\n\
             ```xml\n<workflow_task action=\"complete_phase\" task_id=\"{task_id}\" phase=\"{phase_id}\" />\n```\n\n\
             IMPORTANT: You MUST emit the `complete_phase` tag when done. \
             The workflow cannot advance to the next phase until you do.",
            output_dir = output_dir,
            task_id = task_id,
            phase_id = phase.id,
        ));
        sections.push(guidance);

        Ok(sections.join("\n\n"))
    }

    /// Mark a phase as running in the task snapshot.
    pub async fn start_phase(
        &self,
        task_id: &str,
        phase_id: &str,
    ) -> Result<(), AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;
        snapshot.phases.insert(
            phase_id.to_string(),
            ao_protocol::workflow::PhaseState {
                status: ao_protocol::workflow::PhaseStatus::Running,
                started_at: Some(chrono::Utc::now()),
                completed_at: None,
                skipped_at: None,
                reason: None,
                error: None,
                failed_at: None,
                paused_reason: None,
                input_tokens: None,
                output_tokens: None,
            },
        );
        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;
        Ok(())
    }

    /// Mark a phase as completed.
    /// Validates that all declared outputs exist before marking complete.
    pub async fn complete_phase(
        &self,
        task_id: &str,
        phase_id: &str,
    ) -> Result<(), AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;

        // Reject completion if the task has been stopped
        if snapshot.status == TaskStatus::Stopped {
            tracing::info!(
                task_id = %task_id,
                phase_id = %phase_id,
                "Phase completion rejected — task is stopped"
            );
            return Err(AoError::Internal(format!(
                "Cannot complete phase '{}': task is stopped",
                phase_id
            )));
        }

        // Validate declared outputs exist before allowing completion
        let registry = self.workflow_registry.read().await;
        if let Some(definition) = registry.get_definition(&snapshot.workflow) {
            if let Some(phase_def) = definition.phases.iter().find(|p| p.id == phase_id) {
                let task_output_dir = self
                    .task_store
                    .base_path()
                    .join(task_id)
                    .join("output");
                for output_def in &phase_def.outputs {
                    let default_filename = format!("{}.txt", output_def.id);
                    let filename = output_def.filename.as_deref().unwrap_or(&default_filename);
                    let output_path = task_output_dir.join(filename);
                    if !tokio::fs::try_exists(&output_path).await.unwrap_or(false) {
                        let found = list_output_dir_files(&task_output_dir).await;
                        let error_msg = format!(
                            "Cannot complete phase '{}': missing expected output '{}'.{}",
                            phase_id, filename, found
                        );

                        // Check if this phase was actually Running (started by the workflow runner).
                        // If it was Running, this is a real execution failure — mark it failed.
                        // If it wasn't Running (premature completion from chat agent), just reject
                        // without marking failed so the workflow runner can still execute it later.
                        let was_running = snapshot
                            .phases
                            .get(phase_id)
                            .map(|s| matches!(s.status, PhaseStatus::Running))
                            .unwrap_or(false);

                        if was_running {
                            tracing::error!(
                                task_id = %task_id,
                                phase_id = %phase_id,
                                missing_output = %filename,
                                "Phase completion rejected — output not found"
                            );
                            drop(registry);
                            self.fail_phase(task_id, phase_id, &error_msg).await?;
                        } else {
                            tracing::warn!(
                                task_id = %task_id,
                                phase_id = %phase_id,
                                missing_output = %filename,
                                "Phase completion rejected — output not found (phase not running, skipping failure)"
                            );
                        }
                        return Err(AoError::Internal(error_msg));
                    }
                }
            }
        }
        drop(registry);

        snapshot.phases.insert(
            phase_id.to_string(),
            PhaseState {
                status: PhaseStatus::Completed,
                completed_at: Some(Utc::now()),
                skipped_at: None,
                started_at: snapshot
                    .phases
                    .get(phase_id)
                    .and_then(|s| s.started_at),
                reason: None,
                error: None,
                failed_at: None,
                paused_reason: None,
                input_tokens: None,
                output_tokens: None,
            },
        );

        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        self.event_bus
            .emit(
                task_id,
                &format!("workflow:{}", snapshot.workflow),
                None,
                AgentEventPayload::PhaseCompleted {
                    task_id: task_id.to_string(),
                    phase_id: phase_id.to_string(),
                },
            )
            .await;

        tracing::info!(
            task_id = %task_id,
            phase_id = %phase_id,
            "Phase completed"
        );

        // Check if workflow is now complete
        self.check_workflow_complete(task_id, &snapshot.workflow)
            .await?;

        Ok(())
    }

    /// Mark a phase as skipped.
    pub async fn skip_phase(
        &self,
        task_id: &str,
        phase_id: &str,
        reason: &str,
    ) -> Result<(), AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;

        snapshot.phases.insert(
            phase_id.to_string(),
            PhaseState {
                status: PhaseStatus::Skipped,
                completed_at: None,
                skipped_at: Some(Utc::now()),
                started_at: snapshot
                    .phases
                    .get(phase_id)
                    .and_then(|s| s.started_at),
                reason: Some(reason.to_string()),
                error: None,
                failed_at: None,
                paused_reason: None,
                input_tokens: None,
                output_tokens: None,
            },
        );

        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        self.event_bus
            .emit(
                task_id,
                &format!("workflow:{}", snapshot.workflow),
                None,
                AgentEventPayload::PhaseSkipped {
                    task_id: task_id.to_string(),
                    phase_id: phase_id.to_string(),
                    reason: reason.to_string(),
                },
            )
            .await;

        tracing::info!(
            task_id = %task_id,
            phase_id = %phase_id,
            reason = %reason,
            "Phase skipped"
        );

        // Check if workflow is now complete
        self.check_workflow_complete(task_id, &snapshot.workflow)
            .await?;

        Ok(())
    }

    /// Mark a phase as failed with an error message.
    pub async fn fail_phase(
        &self,
        task_id: &str,
        phase_id: &str,
        error: &str,
    ) -> Result<(), AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;

        snapshot.phases.insert(
            phase_id.to_string(),
            PhaseState {
                status: PhaseStatus::Failed,
                completed_at: None,
                skipped_at: None,
                started_at: snapshot
                    .phases
                    .get(phase_id)
                    .and_then(|s| s.started_at),
                reason: None,
                error: Some(error.to_string()),
                failed_at: Some(Utc::now()),
                paused_reason: None,
                input_tokens: None,
                output_tokens: None,
            },
        );

        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        self.event_bus
            .emit(
                task_id,
                &format!("workflow:{}", snapshot.workflow),
                None,
                AgentEventPayload::PhaseFailed {
                    task_id: task_id.to_string(),
                    phase_id: phase_id.to_string(),
                    error: error.to_string(),
                },
            )
            .await;

        tracing::error!(
            task_id = %task_id,
            phase_id = %phase_id,
            error = %error,
            "Phase failed"
        );

        self.check_workflow_complete(task_id, &snapshot.workflow)
            .await?;

        Ok(())
    }

    /// Validate that the output filename the agent is trying to write matches
    /// a declared output in the workflow definition. If the agent used an output
    /// id instead of the filename, returns Err with a correction message for the agent.
    /// Returns Ok(()) if the filename is valid (matches a declared output or the
    /// phase has no declared outputs, allowing extra files).
    pub async fn validate_output_filename(
        &self,
        task_id: &str,
        phase_id: &str,
        filename: &str,
    ) -> Result<(), String> {
        let snapshot = match self.task_store.read_task_snapshot(task_id).await {
            Ok(s) => s,
            Err(_) => return Ok(()), // Can't validate, allow the write
        };
        let registry = self.workflow_registry.read().await;
        let definition = match registry.get_definition(&snapshot.workflow) {
            Some(def) => def,
            None => return Ok(()),
        };
        let phase_def = match definition.phases.iter().find(|p| p.id == phase_id) {
            Some(p) => p,
            None => return Ok(()), // Unknown phase, allow the write
        };

        if phase_def.outputs.is_empty() {
            return Ok(()); // No declared outputs, anything goes
        }

        // Check if filename matches any declared output filename
        for output in &phase_def.outputs {
            let default_filename = format!("{}.txt", output.id);
            let declared_filename = output
                .filename
                .as_deref()
                .unwrap_or(&default_filename);
            if filename == declared_filename {
                return Ok(()); // Exact match
            }
        }

        // Filename didn't match — check if the agent used an output id instead
        // and build a helpful correction message
        let mut correction = format!(
            "write_output rejected: `{}` is not a valid output filename for phase `{}`.",
            filename, phase_id
        );

        // Check if it looks like an output id
        for output in &phase_def.outputs {
            let id_as_filename = format!("{}.md", output.id);
            let id_as_txt = format!("{}.txt", output.id);
            if filename == output.id || filename == id_as_filename || filename == id_as_txt {
                let default_fn = format!("{}.txt", output.id);
                let declared_filename = output
                    .filename
                    .as_deref()
                    .unwrap_or(&default_fn);
                correction.push_str(&format!(
                    " It looks like you used the output id `{}`. The correct filename is `{}`. \
                     Please retry with output=\"{}\".",
                    output.id, declared_filename, declared_filename
                ));
                return Err(correction);
            }
        }

        // Unknown filename — list valid options
        let valid: Vec<String> = phase_def
            .outputs
            .iter()
            .map(|o| {
                o.filename
                    .clone()
                    .unwrap_or_else(|| format!("{}.txt", o.id))
            })
            .collect();
        correction.push_str(&format!(
            " Valid output filenames for this phase: {}. Please retry with the correct filename.",
            valid.join(", ")
        ));
        Err(correction)
    }

    /// Write content to the task output directory.
    pub async fn write_phase_output(
        &self,
        task_id: &str,
        filename: &str,
        content: &str,
    ) -> Result<(), AoError> {
        self.task_store
            .write_output(task_id, filename, content)
            .await?;

        tracing::debug!(
            task_id = %task_id,
            filename = %filename,
            "Wrote phase output"
        );

        Ok(())
    }

    /// Return the required output filenames for a specific phase.
    pub async fn phase_required_outputs(
        &self,
        task_id: &str,
        phase_id: &str,
    ) -> Result<Vec<String>, AoError> {
        let snapshot = self.task_store.read_task_snapshot(task_id).await?;
        let registry = self.workflow_registry.read().await;
        let definition = registry
            .get_definition(&snapshot.workflow)
            .ok_or_else(|| {
                AoError::Internal(format!(
                    "Workflow '{}' not found in registry",
                    snapshot.workflow
                ))
            })?;
        let phase_def = definition
            .phases
            .iter()
            .find(|p| p.id == phase_id)
            .ok_or_else(|| {
                AoError::Internal(format!(
                    "Phase '{}' not found in workflow '{}'",
                    phase_id, snapshot.workflow
                ))
            })?;
        let filenames = phase_def
            .outputs
            .iter()
            .map(|o| {
                o.filename
                    .clone()
                    .unwrap_or_else(|| format!("{}.txt", o.id))
            })
            .collect();
        Ok(filenames)
    }

    /// Build a phase-progress summary for WorkflowActionWriteOutput.
    ///
    /// Finds the current active phase for the task, gets its required outputs,
    /// scans the output directory, and returns a formatted progress string.
    /// Returns None on any failure so the caller falls back to the plain message.
    pub async fn phase_write_progress_summary(
        &self,
        task_id: &str,
        filename_just_written: &str,
    ) -> Option<String> {
        // Determine the current active phase: prefer Running, fall back to next pending.
        let phase_id = match self.task_store.read_task_snapshot(task_id).await {
            Ok(snapshot) => {
                let running = snapshot
                    .phases
                    .iter()
                    .find(|(_, s)| matches!(s.status, PhaseStatus::Running))
                    .map(|(id, _)| id.clone());
                if let Some(id) = running {
                    id
                } else {
                    match self.get_next_phase(task_id).await {
                        Ok(Some(phase_def)) => phase_def.id,
                        Ok(None) => return None,
                        Err(e) => {
                            tracing::warn!(
                                task_id = %task_id,
                                error = %e,
                                "phase_write_progress_summary: failed to get next phase"
                            );
                            return None;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "phase_write_progress_summary: failed to read snapshot"
                );
                return None;
            }
        };

        let required = match self.phase_required_outputs(task_id, &phase_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    phase_id = %phase_id,
                    error = %e,
                    "phase_write_progress_summary: failed to get required outputs"
                );
                return None;
            }
        };

        if required.is_empty() {
            return None;
        }

        let output_dir = self
            .task_store
            .base_path()
            .join(task_id)
            .join("output");

        let mut present: Vec<String> = Vec::new();
        match tokio::fs::read_dir(&output_dir).await {
            Ok(mut entries) => {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name();
                    let name_str = match name.to_str() {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    // Skip hidden files (e.g. .DS_Store)
                    if name_str.starts_with('.') {
                        continue;
                    }
                    if required.contains(&name_str) {
                        present.push(name_str);
                    }
                }
            }
            Err(_) => {
                // Directory not readable — assume only the file just written is present
                if required.contains(&filename_just_written.to_string()) {
                    present.push(filename_just_written.to_string());
                }
            }
        }

        let total = required.len();
        let found = present.len();

        if found >= total {
            Some(format!(
                "Phase '{}' now has all {} required output{}. Call WorkflowActionCompletePhase to advance.",
                phase_id,
                total,
                if total == 1 { "" } else { "s" }
            ))
        } else {
            let missing: Vec<String> = required
                .into_iter()
                .filter(|r| !present.contains(r))
                .collect();
            Some(format!(
                "Phase '{}' now has {}/{} required outputs. Still missing: [{}].",
                phase_id,
                found,
                total,
                missing.join(", ")
            ))
        }
    }

    /// Read the current task snapshot.
    pub async fn get_task_state(
        &self,
        task_id: &str,
    ) -> Result<TaskSnapshot, AoError> {
        self.task_store.read_task_snapshot(task_id).await
    }

    /// Write an updated task snapshot.
    pub async fn write_task_snapshot(
        &self,
        task_id: &str,
        snapshot: &TaskSnapshot,
    ) -> Result<(), AoError> {
        self.task_store.write_task_snapshot(task_id, snapshot).await
    }

    /// List all task IDs from the task store.
    pub async fn list_task_ids(&self) -> Result<Vec<String>, AoError> {
        self.task_store.list_tasks().await
    }

    /// Read all phase messages for a task phase.
    pub async fn read_phase_messages(
        &self,
        task_id: &str,
        phase_id: &str,
    ) -> Result<Vec<ao_protocol::transcript::TranscriptEntry>, AoError> {
        self.task_store.read_phase_messages(task_id, phase_id).await
    }

    /// Append a message to a task phase's message log.
    pub async fn append_phase_message(
        &self,
        task_id: &str,
        phase_id: &str,
        entry: &ao_protocol::transcript::TranscriptEntry,
    ) -> Result<(), AoError> {
        self.task_store
            .append_phase_message(task_id, phase_id, entry)
            .await
    }

    /// Delete a task entirely from disk.
    pub async fn delete_task(&self, task_id: &str) -> Result<(), AoError> {
        // Verify task exists first
        self.task_store.read_task_snapshot(task_id).await?;
        self.task_store.delete_task(task_id).await
    }

    /// Archive a task by setting its status to Archived.
    pub async fn archive_task(&self, task_id: &str) -> Result<(), AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;
        snapshot.status = ao_protocol::workflow::TaskStatus::Archived;
        self.task_store.write_task_snapshot(task_id, &snapshot).await
    }

    /// Read a task output file.
    pub async fn read_task_output(
        &self,
        task_id: &str,
        filename: &str,
    ) -> Result<String, AoError> {
        self.task_store.read_output(task_id, filename).await
    }

    /// Accumulate token usage for a phase (adds to existing totals).
    pub async fn accumulate_phase_tokens(
        &self,
        task_id: &str,
        phase_id: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<(), AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;
        if let Some(state) = snapshot.phases.get_mut(phase_id) {
            let prev_in = state.input_tokens.unwrap_or(0);
            let prev_out = state.output_tokens.unwrap_or(0);
            state.input_tokens = Some(prev_in + input_tokens);
            state.output_tokens = Some(prev_out + output_tokens);
            self.task_store
                .write_task_snapshot(task_id, &snapshot)
                .await?;
        }
        Ok(())
    }

    /// Write a task output file.
    pub async fn write_task_output(
        &self,
        task_id: &str,
        filename: &str,
        content: &str,
    ) -> Result<(), AoError> {
        self.task_store.write_output(task_id, filename, content).await
    }

    /// Start a pending task. Validates task is in Pending state, transitions to Running,
    /// and emits a WorkflowTaskStarted event.
    pub async fn start_task(&self, task_id: &str) -> Result<(), AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;

        if snapshot.status != TaskStatus::Pending {
            return Err(AoError::Internal(format!(
                "Cannot start task '{}': status is '{}', expected 'pending'",
                task_id,
                serde_json::to_value(&snapshot.status)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("unknown")
            )));
        }

        snapshot.status = TaskStatus::Running;
        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        self.event_bus
            .emit(
                task_id,
                &format!("workflow:{}", snapshot.workflow),
                None,
                AgentEventPayload::WorkflowTaskStarted {
                    task_id: task_id.to_string(),
                },
            )
            .await;

        tracing::info!(task_id = %task_id, "Task started");

        Ok(())
    }

    /// Emit a WorkflowTaskStarted SSE event for a task (used on resume).
    pub async fn emit_task_started(&self, task_id: &str) {
        if let Ok(snapshot) = self.task_store.read_task_snapshot(task_id).await {
            self.event_bus
                .emit(
                    task_id,
                    &format!("workflow:{}", snapshot.workflow),
                    None,
                    AgentEventPayload::WorkflowTaskStarted {
                        task_id: task_id.to_string(),
                    },
                )
                .await;
        }
    }

    /// Stop a task (agent-facing). Marks all Running phases as Stopped, sets
    /// task status to Stopped, persists, emits WorkflowTaskStopped event.
    /// Returns the task output directory path for use in success messages.
    /// Callers are responsible for checking terminal-state idempotency beforehand.
    pub async fn stop_task(&self, task_id: &str) -> Result<PathBuf, AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;
        let workflow_id = snapshot.workflow.clone();

        for (_phase_id, phase_state) in snapshot.phases.iter_mut() {
            if matches!(phase_state.status, PhaseStatus::Running) {
                phase_state.status = PhaseStatus::Stopped;
            }
        }

        snapshot.status = TaskStatus::Stopped;

        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        self.event_bus
            .emit(
                task_id,
                &format!("workflow:{}", workflow_id),
                None,
                AgentEventPayload::WorkflowTaskStopped {
                    task_id: task_id.to_string(),
                },
            )
            .await;

        tracing::info!(task_id = %task_id, "Task stopped via WorkflowActionStop");

        let output_dir = self.task_store.base_path().join(task_id).join("output");
        Ok(output_dir)
    }

    /// Reopen a terminal task (Completed, Failed, or Stopped) to a specific phase
    /// for re-run. Sets task status to Pending, removes the target phase from the
    /// phases map so `get_next_phase` will schedule it again. All other phase states
    /// and output files are preserved. Returns the count of output files.
    pub async fn reopen_task(&self, task_id: &str, phase_id: &str) -> Result<usize, AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;

        match snapshot.status {
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Stopped => {}
            _ => {
                let status_str = serde_json::to_value(&snapshot.status)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();
                return Err(AoError::ValidationError(format!(
                    "Cannot reopen task '{}': task must be in a terminal state \
                     (completed, failed, or stopped), but current status is '{}'.",
                    task_id, status_str
                )));
            }
        }

        let workflow_id = snapshot.workflow.clone();
        {
            let registry = self.workflow_registry.read().await;
            let definition = registry.get_definition(&workflow_id).ok_or_else(|| {
                AoError::Internal(format!(
                    "Workflow '{}' not found in registry",
                    workflow_id
                ))
            })?;

            if !definition.phases.iter().any(|p| p.id == phase_id) {
                let valid: Vec<&str> = definition.phases.iter().map(|p| p.id.as_str()).collect();
                return Err(AoError::ValidationError(format!(
                    "Cannot reopen task '{}': phase '{}' does not exist in workflow '{}'. \
                     Valid phase IDs: [{}].",
                    task_id,
                    phase_id,
                    workflow_id,
                    valid.join(", ")
                )));
            }
        }

        snapshot.status = TaskStatus::Pending;
        snapshot.phases.remove(phase_id);

        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        self.event_bus
            .emit(
                task_id,
                &format!("workflow:{}", workflow_id),
                None,
                AgentEventPayload::WorkflowTaskReopened {
                    task_id: task_id.to_string(),
                    phase_id: phase_id.to_string(),
                },
            )
            .await;

        tracing::info!(task_id = %task_id, phase_id = %phase_id, "Task reopened via WorkflowActionReopen");

        let output_dir = self.task_store.base_path().join(task_id).join("output");
        Ok(count_output_dir_files(&output_dir).await)
    }

    /// Cancel a running task by stopping the current phase and marking the task as stopped.
    pub async fn cancel_task(&self, task_id: &str) -> Result<(), AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;

        // Mark the currently running phase as stopped, leave pending phases untouched
        let workflow_id = snapshot.workflow.clone();

        for (_phase_id, phase_state) in snapshot.phases.iter_mut() {
            if matches!(phase_state.status, PhaseStatus::Running) {
                phase_state.status = PhaseStatus::Stopped;
            }
        }

        snapshot.status = TaskStatus::Stopped;

        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        self.event_bus
            .emit(
                task_id,
                &format!("workflow:{}", workflow_id),
                None,
                AgentEventPayload::WorkflowTaskStopped {
                    task_id: task_id.to_string(),
                },
            )
            .await;

        tracing::info!(task_id = %task_id, "Task stopped");

        Ok(())
    }

    /// Check if a phase is a folder phase (path resolves to a directory).
    pub async fn is_folder_phase(
        &self,
        workflow_id: &str,
        phase: &PhaseDefinition,
    ) -> bool {
        let phase_path = self
            .workflow_store
            .base_path()
            .join(workflow_id)
            .join(&phase.path);
        tokio::fs::metadata(&phase_path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
    }

    /// Execute a folder-based phase by running its run.sh script.
    /// Folder phases are detected by checking if phase.path resolves to a directory.
    pub async fn execute_folder_phase(
        &self,
        task_id: &str,
        phase: &PhaseDefinition,
    ) -> Result<(), AoError> {
        let snapshot = self.task_store.read_task_snapshot(task_id).await?;
        let workflow_id = &snapshot.workflow;

        let phase_dir = self
            .workflow_store
            .base_path()
            .join(workflow_id)
            .join(&phase.path);

        let run_script = phase_dir.join("run.sh");
        if !tokio::fs::try_exists(&run_script).await.unwrap_or(false) {
            return Err(AoError::Internal(format!(
                "Folder phase '{}' missing run.sh in {}",
                phase.id,
                phase_dir.display()
            )));
        }

        // Persist phase as running in snapshot
        self.start_phase(task_id, &phase.id).await?;

        // Emit PhaseStarted
        self.event_bus
            .emit(
                task_id,
                &format!("workflow:{}", workflow_id),
                None,
                AgentEventPayload::PhaseStarted {
                    task_id: task_id.to_string(),
                    phase_id: phase.id.clone(),
                    phase_name: phase.name.clone(),
                },
            )
            .await;

        // Build env vars for the phase script
        let task_output_dir = self.task_store.base_path().join(task_id).join("output");
        let task_dir = self.task_store.base_path().join(task_id);
        let status_file = task_dir.join("_workflow_status.json");

        let mut env_vars: Vec<(String, String)> = vec![
            ("WORKFLOW_TASK_ID".to_string(), task_id.to_string()),
            ("WORKFLOW_PHASE_ID".to_string(), phase.id.clone()),
            (
                "WORKFLOW_OUTPUT_DIR".to_string(),
                task_output_dir.to_string_lossy().to_string(),
            ),
            (
                "WORKFLOW_STATUS_FILE".to_string(),
                status_file.to_string_lossy().to_string(),
            ),
        ];

        // Add working directory if set
        if let Some(ref wd) = snapshot.working_directory {
            env_vars.push(("WORKFLOW_WORKING_DIR".to_string(), wd.clone()));
        }

        // Add input paths as WORKFLOW_INPUT_{INPUT_ID} env vars
        for input in &phase.inputs {
            if let (Some(ref from_phase), Some(ref from_output)) =
                (&input.from_phase, &input.from_output)
            {
                let filename = self
                    .resolve_output_filename(workflow_id, from_phase, from_output)
                    .await;
                let input_path = task_output_dir.join(&filename);
                let env_key = format!("WORKFLOW_INPUT_{}", input.id.to_uppercase());
                env_vars.push((env_key, input_path.to_string_lossy().to_string()));
            }
        }

        tracing::info!(
            task_id = %task_id,
            phase_id = %phase.id,
            phase_dir = %phase_dir.display(),
            env_vars = ?env_vars,
            "Executing folder phase"
        );

        // Execute run.sh with env vars, polling _workflow_status.json for progress
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("run.sh").current_dir(&phase_dir);
        for (key, val) in &env_vars {
            cmd.env(key, val);
        }
        let mut child = cmd.spawn().map_err(|e| {
            AoError::Process(format!(
                "Failed to spawn run.sh for phase '{}': {}",
                phase.id, e
            ))
        })?;

        // Poll _workflow_status.json while the process runs
        let status_file_path = status_file.clone();
        let event_bus = self.event_bus.clone();
        let poll_task_id = task_id.to_string();
        let poll_phase_id = phase.id.clone();
        let poll_workflow_id = workflow_id.to_string();
        let mut last_content = String::new();

        let poll_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                if let Ok(content) = tokio::fs::read_to_string(&status_file_path).await {
                    if content != last_content {
                        last_content = content.clone();
                        // Parse and emit progress event
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                            let status = val
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("running")
                                .to_string();
                            let message =
                                val.get("message").and_then(|v| v.as_str()).map(String::from);
                            let percent =
                                val.get("percent").and_then(|v| v.as_u64()).map(|p| p as u8);

                            event_bus
                                .emit(
                                    &poll_task_id,
                                    &format!("workflow:{}", poll_workflow_id),
                                    None,
                                    AgentEventPayload::WorkflowPhaseProgress {
                                        task_id: poll_task_id.clone(),
                                        phase_id: poll_phase_id.clone(),
                                        status,
                                        message,
                                        percent,
                                    },
                                )
                                .await;
                        }
                    }
                }
            }
        });

        let exit_status = child.wait().await.map_err(|e| {
            AoError::Process(format!(
                "Failed to wait for run.sh for phase '{}': {}",
                phase.id, e
            ))
        })?;

        // Stop polling
        poll_handle.abort();

        // Read final status for token usage before cleanup
        if let Ok(content) = tokio::fs::read_to_string(&status_file).await {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                let input_tokens = val.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let output_tokens = val.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                if input_tokens > 0 || output_tokens > 0 {
                    if let Err(e) = self
                        .accumulate_phase_tokens(task_id, &phase.id, input_tokens, output_tokens)
                        .await
                    {
                        tracing::warn!(
                            task_id = %task_id,
                            phase_id = %phase.id,
                            "Failed to write folder phase token usage: {}",
                            e
                        );
                    }
                }
            }
        }

        // Clean up status file
        let _ = tokio::fs::remove_file(&status_file).await;

        if !exit_status.success() {
            let exit_code = exit_status.code().unwrap_or(-1);
            let error_msg = format!(
                "Phase '{}' run.sh failed with exit code {}",
                phase.id, exit_code
            );
            tracing::error!(
                task_id = %task_id,
                phase_id = %phase.id,
                exit_code = exit_code,
                "Folder phase script failed"
            );
            self.fail_phase(task_id, &phase.id, &error_msg).await?;
            return Err(AoError::Process(error_msg));
        }

        // Verify all declared outputs exist
        for output_def in &phase.outputs {
            let default_filename = format!("{}.txt", output_def.id);
            let filename = output_def.filename.as_deref().unwrap_or(&default_filename);
            let output_path = task_output_dir.join(filename);
            if !tokio::fs::try_exists(&output_path).await.unwrap_or(false) {
                let found = list_output_dir_files(&task_output_dir).await;
                let error_msg = format!(
                    "Phase '{}' missing expected output: {}.{}",
                    phase.id, filename, found
                );
                self.fail_phase(task_id, &phase.id, &error_msg).await?;
                return Err(AoError::Internal(error_msg));
            }
        }

        // Mark phase as completed
        self.complete_phase(task_id, &phase.id).await?;

        tracing::info!(
            task_id = %task_id,
            phase_id = %phase.id,
            "Folder phase completed"
        );

        Ok(())
    }

    /// Advance to the next phase that requires agent execution.
    /// Automatically executes any folder phases in sequence, returning
    /// the context string for the first file-based phase encountered,
    /// or None if all remaining phases are folder phases and complete successfully.
    pub async fn advance_to_next_agent_phase(
        &self,
        task_id: &str,
    ) -> Result<Option<String>, AoError> {
        loop {
            let Some(phase) = self.get_next_phase(task_id).await? else {
                return Ok(None); // All phases done
            };

            let snapshot = self.task_store.read_task_snapshot(task_id).await?;
            let workflow_id = snapshot.workflow.clone();

            if self.is_folder_phase(&workflow_id, &phase).await {
                // Execute folder phase directly
                self.execute_folder_phase(task_id, &phase).await?;
                // Loop to check next phase
            } else {
                // File phase — build context for agent
                let context = self.build_phase_context(task_id, &phase).await?;
                return Ok(Some(context));
            }
        }
    }

    /// Resolve the absolute path of a phase directory (for folder phases).
    pub fn resolve_phase_path(&self, workflow_id: &str, phase_path: &str) -> PathBuf {
        self.workflow_store
            .base_path()
            .join(workflow_id)
            .join(phase_path)
    }

    /// Check whether all declared inputs for a phase are available as output files
    /// from their source phases. Returns a list of missing input names.
    pub async fn check_inputs_available(
        &self,
        task_id: &str,
        phase: &PhaseDefinition,
    ) -> Result<Vec<String>, AoError> {
        let snapshot = self.task_store.read_task_snapshot(task_id).await?;
        let task_output_dir = self.task_store.base_path().join(task_id).join("output");

        let mut missing = Vec::new();

        for input in &phase.inputs {
            if let (Some(from_phase), Some(from_output)) =
                (&input.from_phase, &input.from_output)
            {
                let filename = self
                    .resolve_output_filename(&snapshot.workflow, from_phase, from_output)
                    .await;
                let output_path = task_output_dir.join(&filename);
                if !tokio::fs::try_exists(&output_path).await.unwrap_or(false) {
                    missing.push(input.id.clone());
                }
            }
        }

        Ok(missing)
    }

    /// Mark a phase as paused with a reason. Emits PhasePaused event.
    /// The task remains in Running status.
    pub async fn pause_phase(
        &self,
        task_id: &str,
        phase_id: &str,
        reason: &str,
    ) -> Result<(), AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;

        snapshot.phases.insert(
            phase_id.to_string(),
            PhaseState {
                status: PhaseStatus::Paused,
                completed_at: None,
                skipped_at: None,
                started_at: snapshot
                    .phases
                    .get(phase_id)
                    .and_then(|s| s.started_at),
                reason: None,
                error: None,
                failed_at: None,
                paused_reason: Some(reason.to_string()),
                input_tokens: None,
                output_tokens: None,
            },
        );

        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        self.event_bus
            .emit(
                task_id,
                &format!("workflow:{}", snapshot.workflow),
                None,
                AgentEventPayload::PhasePaused {
                    task_id: task_id.to_string(),
                    phase_id: phase_id.to_string(),
                    reason: reason.to_string(),
                },
            )
            .await;

        tracing::warn!(
            task_id = %task_id,
            phase_id = %phase_id,
            reason = %reason,
            "Phase paused"
        );

        Ok(())
    }

    /// Clear all paused phases from the task snapshot by removing them from
    /// the phases map so they are re-evaluated by get_next_phase().
    /// Returns true if any paused phase was found and cleared.
    pub async fn clear_paused_phases(&self, task_id: &str) -> Result<bool, AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;
        let mut changed = false;

        // 1. Clear paused phases (remove from map so get_next_phase re-evaluates them)
        let paused_ids: Vec<String> = snapshot
            .phases
            .iter()
            .filter(|(_, state)| matches!(state.status, PhaseStatus::Paused))
            .map(|(id, _)| id.clone())
            .collect();

        if !paused_ids.is_empty() {
            for id in &paused_ids {
                snapshot.phases.remove(id);
            }
            changed = true;
            tracing::info!(
                task_id = %task_id,
                cleared = ?paused_ids,
                "Cleared paused phases for resume"
            );
        }

        // 2. Reconcile failed phases whose expected outputs now exist on disk.
        //    This handles the case where a phase failed due to missing output,
        //    the output was later provided (e.g. manually), and the task is resumed.
        let failed_ids: Vec<String> = snapshot
            .phases
            .iter()
            .filter(|(_, state)| matches!(state.status, PhaseStatus::Failed))
            .map(|(id, _)| id.clone())
            .collect();

        if !failed_ids.is_empty() {
            let registry = self.workflow_registry.read().await;
            if let Some(definition) = registry.get_definition(&snapshot.workflow) {
                let task_output_dir = self
                    .task_store
                    .base_path()
                    .join(task_id)
                    .join("output");

                for failed_id in &failed_ids {
                    if let Some(phase_def) =
                        definition.phases.iter().find(|p| p.id == *failed_id)
                    {
                        // Check if all declared outputs now exist
                        let all_outputs_present = {
                            let mut all_present = true;
                            for output_def in &phase_def.outputs {
                                let default_filename = format!("{}.txt", output_def.id);
                                let filename =
                                    output_def.filename.as_deref().unwrap_or(&default_filename);
                                let output_path = task_output_dir.join(filename);
                                if !tokio::fs::try_exists(&output_path)
                                    .await
                                    .unwrap_or(false)
                                {
                                    all_present = false;
                                    break;
                                }
                            }
                            all_present
                        };

                        if all_outputs_present {
                            snapshot.phases.insert(
                                failed_id.clone(),
                                PhaseState {
                                    status: PhaseStatus::Completed,
                                    completed_at: Some(Utc::now()),
                                    skipped_at: None,
                                    started_at: snapshot
                                        .phases
                                        .get(failed_id)
                                        .and_then(|s| s.started_at),
                                    reason: None,
                                    error: None,
                                    failed_at: None,
                                    paused_reason: None,
                                    input_tokens: None,
                                    output_tokens: None,
                                },
                            );
                            changed = true;

                            tracing::info!(
                                task_id = %task_id,
                                phase_id = %failed_id,
                                "Reconciled failed phase to completed — outputs now present"
                            );

                            // Emit PhaseCompleted so frontend updates
                            self.event_bus
                                .emit(
                                    task_id,
                                    &format!("workflow:{}", snapshot.workflow),
                                    None,
                                    AgentEventPayload::PhaseCompleted {
                                        task_id: task_id.to_string(),
                                        phase_id: failed_id.clone(),
                                    },
                                )
                                .await;
                        }
                    }
                }
            }
        }

        if !changed {
            return Ok(false);
        }

        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        Ok(true)
    }

    /// Clear stopped phases from the task snapshot by removing them from
    /// the phases map so they are re-evaluated by get_next_phase().
    /// Also resets the task status from Stopped back to Running.
    /// Returns true if any stopped phase was found and cleared.
    pub async fn clear_stopped_phases(&self, task_id: &str) -> Result<bool, AoError> {
        let mut snapshot = self.task_store.read_task_snapshot(task_id).await?;

        let stopped_ids: Vec<String> = snapshot
            .phases
            .iter()
            .filter(|(_, state)| matches!(state.status, PhaseStatus::Stopped))
            .map(|(id, _)| id.clone())
            .collect();

        if stopped_ids.is_empty() {
            return Ok(false);
        }

        for id in &stopped_ids {
            snapshot.phases.remove(id);
        }

        // Reset task status back to Running so the queue manager picks it up
        snapshot.status = TaskStatus::Running;

        self.task_store
            .write_task_snapshot(task_id, &snapshot)
            .await?;

        tracing::info!(
            task_id = %task_id,
            cleared = ?stopped_ids,
            "Cleared stopped phases for resume"
        );

        Ok(true)
    }

    /// Check if all phases in the workflow are done (completed or skipped).
    /// If so, emit WorkflowCompleted event.
    async fn check_workflow_complete(
        &self,
        task_id: &str,
        workflow_id: &str,
    ) -> Result<(), AoError> {
        let snapshot = self.task_store.read_task_snapshot(task_id).await?;
        let registry = self.workflow_registry.read().await;
        let Some(definition) = registry.get_definition(workflow_id) else {
            return Ok(());
        };

        let all_done = definition.phases.iter().all(|phase| {
            snapshot
                .phases
                .get(&phase.id)
                .map(|s| {
                    matches!(
                        s.status,
                        PhaseStatus::Completed | PhaseStatus::Skipped | PhaseStatus::Failed
                    )
                })
                .unwrap_or(false)
        });

        if all_done {
            let failed_phases: Vec<String> = definition
                .phases
                .iter()
                .filter(|phase| {
                    snapshot
                        .phases
                        .get(&phase.id)
                        .map(|s| matches!(s.status, PhaseStatus::Failed))
                        .unwrap_or(false)
                })
                .map(|p| p.id.clone())
                .collect();

            let has_failures = !failed_phases.is_empty();

            drop(registry);

            if has_failures {

                let error = format!(
                    "Workflow failed: phases [{}] failed",
                    failed_phases.join(", ")
                );

                self.event_bus
                    .emit(
                        task_id,
                        &format!("workflow:{}", workflow_id),
                        None,
                        AgentEventPayload::WorkflowTaskFailed {
                            task_id: task_id.to_string(),
                            error: error.clone(),
                        },
                    )
                    .await;

                tracing::warn!(
                    task_id = %task_id,
                    workflow_id = %workflow_id,
                    error = %error,
                    "Workflow failed"
                );
            } else {
                self.event_bus
                    .emit(
                        task_id,
                        &format!("workflow:{}", workflow_id),
                        None,
                        AgentEventPayload::WorkflowCompleted {
                            task_id: task_id.to_string(),
                        },
                    )
                    .await;

                tracing::info!(
                    task_id = %task_id,
                    workflow_id = %workflow_id,
                    "Workflow completed"
                );
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl ao_engine_tools_core::WorkflowRunnerHandle for WorkflowRunner {
    async fn create_task(
        &self,
        workflow_id: &str,
        project_name: &str,
        working_directory: Option<String>,
        context: Option<String>,
    ) -> Result<String, AoError> {
        WorkflowRunner::create_task(self, workflow_id, project_name, working_directory, context)
            .await
    }

    async fn build_create_summary(
        &self,
        task_id: &str,
        workflow_id: &str,
    ) -> Result<String, AoError> {
        WorkflowRunner::build_create_summary(self, task_id, workflow_id).await
    }

    async fn write_phase_output(
        &self,
        task_id: &str,
        filename: &str,
        content: &str,
    ) -> Result<(), AoError> {
        WorkflowRunner::write_phase_output(self, task_id, filename, content).await
    }

    async fn complete_phase(&self, task_id: &str, phase_id: &str) -> Result<(), AoError> {
        WorkflowRunner::complete_phase(self, task_id, phase_id).await
    }

    async fn skip_phase(
        &self,
        task_id: &str,
        phase_id: &str,
        reason: &str,
    ) -> Result<(), AoError> {
        WorkflowRunner::skip_phase(self, task_id, phase_id, reason).await
    }

    async fn start_task(&self, task_id: &str) -> Result<(), AoError> {
        WorkflowRunner::start_task(self, task_id).await
    }

    async fn delete_task(&self, task_id: &str) -> Result<(), AoError> {
        WorkflowRunner::delete_task(self, task_id).await
    }

    async fn get_task_state(&self, task_id: &str) -> Result<ao_protocol::workflow::TaskSnapshot, AoError> {
        WorkflowRunner::get_task_state(self, task_id).await
    }

    async fn get_next_phase(
        &self,
        task_id: &str,
    ) -> Result<Option<ao_protocol::workflow::PhaseDefinition>, AoError> {
        WorkflowRunner::get_next_phase(self, task_id).await
    }

    async fn notify_phase_completed(
        &self,
        task_id: &str,
        phase_id: &str,
    ) -> Result<(), AoError> {
        // Read-guard scope must drop before the await on send() so we don't
        // hold the lock across .await — and tokio's RwLock guard is !Send for
        // that reason. Clone the handle out, then drop the guard.
        let handle = self.workflow_queue.read().await.clone();
        match handle {
            Some(h) => h
                .send(WfQueueMsg::PhaseCompleted {
                    task_id: task_id.to_string(),
                    phase_id: phase_id.to_string(),
                })
                .await,
            None => {
                tracing::debug!(
                    task_id = %task_id,
                    phase_id = %phase_id,
                    "notify_phase_completed: no queue handle wired; ignoring"
                );
                Ok(())
            }
        }
    }

    async fn get_workflow_summaries(
        &self,
        ids: Option<&[String]>,
    ) -> Vec<ao_protocol::workflow::WorkflowSummary> {
        let registry = self.workflow_registry.read().await;
        match ids {
            None => registry.list_summaries().into_iter().cloned().collect(),
            Some(ids) => ids
                .iter()
                .filter_map(|id| registry.get_summary(id).cloned())
                .collect(),
        }
    }

    async fn stop_task(&self, task_id: &str) -> Result<std::path::PathBuf, AoError> {
        WorkflowRunner::stop_task(self, task_id).await
    }

    async fn phase_required_outputs(
        &self,
        task_id: &str,
        phase_id: &str,
    ) -> Result<Vec<String>, AoError> {
        WorkflowRunner::phase_required_outputs(self, task_id, phase_id).await
    }

    async fn phase_write_progress_summary(
        &self,
        task_id: &str,
        filename_just_written: &str,
    ) -> Option<String> {
        WorkflowRunner::phase_write_progress_summary(self, task_id, filename_just_written).await
    }

    async fn reopen_task(&self, task_id: &str, phase_id: &str) -> Result<usize, AoError> {
        WorkflowRunner::reopen_task(self, task_id, phase_id).await
    }
}

#[cfg(test)]
mod tests;
