// ---------------------------------------------------------------------------
// Workflow & Task TypeScript interfaces matching the ao-server backend API.
// ---------------------------------------------------------------------------

/** Full workflow definition returned by GET /workflows/{id}. */
export interface WorkflowDefinition {
  id: string;
  name: string;
  version?: string | null;
  description?: string | null;
  phases: PhaseDefinition[];
}

/** A single phase within a workflow definition. */
export type PhaseType = "folder" | "prompt" | "input" | "pause";

export interface InputField {
  name: string;
  label: string;
  placeholder?: string | null;
  description?: string | null;
  required?: boolean;
}

export interface PhaseDefinition {
  id: string;
  name: string;
  intent?: string | null;
  path: string;
  phase_type?: PhaseType | null;
  auto_advance?: boolean;
  schema?: string | null;
  inputs: PhaseInput[];
  outputs: PhaseOutput[];
  fields?: InputField[];
}

/** An input reference for a phase (pulls from a prior phase's output). */
export interface PhaseInput {
  id: string;
  from_phase?: string | null;
  from_output?: string | null;
}

/** A declared output for a phase. */
export interface PhaseOutput {
  id: string;
  filename?: string | null;
  description?: string | null;
}

/** Provenance of a workflow definition. */
export type WorkflowSource = "project" | "user" | "plugin";

/** Lightweight workflow summary returned by GET /workflows. */
export interface WorkflowSummary {
  id: string;
  name: string;
  version?: string | null;
  description?: string | null;
  phase_count?: number;
  source?: WorkflowSource;
  updated_on?: string | null;
  last_run?: string | null;
}

/** Lifecycle status of a workflow task. */
export type TaskStatus = "pending" | "running" | "completed" | "failed" | "archived" | "stopped";

/** Status of a phase within a running task. */
export type PhaseStatus = "completed" | "skipped" | "running" | "failed" | "paused" | "stopped";

/** State of a single phase within a task snapshot. */
export interface PhaseState {
  status: PhaseStatus;
  completed_at?: string | null;
  skipped_at?: string | null;
  started_at?: string | null;
  reason?: string | null;
  error?: string | null;
  failed_at?: string | null;
  paused_reason?: string | null;
  input_tokens?: number | null;
  output_tokens?: number | null;
}

/** Full task snapshot returned by GET /tasks/{id}. */
export interface TaskSnapshot {
  workflow: string;
  workflow_version?: string | null;
  created: string;
  project_name: string;
  working_directory?: string | null;
  context: Record<string, string>;
  phases: Record<string, PhaseState>;
  status?: TaskStatus;
}

/** Lightweight task summary returned by GET /tasks. */
export interface TaskSummary {
  task_id: string;
  workflow: string;
  project_name: string;
  created: string;
  completed_phases: number;
  total_phases: number;
  status: TaskStatus;
  completed_at?: string | null;
  started_at?: string | null;
  /** True when task is running but current phase is paused (awaiting user action). */
  is_paused?: boolean;
}

/** Response from POST /workflows/refresh. */
export interface RefreshResponse {
  count: number;
}

/** Response from POST /workflows/{id}/tasks. */
export interface CreateTaskResponse {
  task_id: string;
}

/** Request body for POST /workflows/{id}/tasks. */
export interface CreateTaskRequest {
  project_name: string;
  working_directory?: string | null;
  context?: string | null;
}
