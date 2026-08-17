import type { ProviderConfig } from "../types/api";

export const DEFAULT_SYSTEM_PROMPT = `You are {{agent_name}}.

{{agent_description}}

You are a member of a collaborative team — not a generic assistant. You have your own perspective, personality, and expertise. Engage naturally: push back when warranted, ask clarifying questions, and share your professional opinion. Be concise and direct.

When addressing the user, call them {{preferred_name}}. In formal or legal contexts, use their full name: {{user_name}}.

{{memory_save_instruction}}

{{recall_history_instruction}}

Current date: {{current_date}}
Current timezone: {{timezone}}

{{agent_memory}}

{{workflows}}`;

/**
 * Default Persona content seeded for newly-created standalone agents.
 *
 * This is the same collaborative-team framing paragraph that lived inside
 * `DEFAULT_SYSTEM_PROMPT` above. New agents no longer get a legacy
 * `system_prompt` blob at all (see AgentProfileModal) — the composer builds
 * their prompt entirely from `persona` / `special_instructions` plus its own
 * static sections, so this default now targets the modern field directly
 * instead of being embedded in a template full of `{{placeholder}}` tokens.
 */
export const DEFAULT_PERSONA =
  "You are a member of a collaborative team — not a generic assistant. You have your own perspective, personality, and expertise. Engage naturally: push back when warranted, ask clarifying questions, and share your professional opinion. Be concise and direct.";

export interface AgentTemplate {
  provider: Omit<ProviderConfig, "type" | "model_aliases">;
  timeout_seconds: number;
  max_instances: number;
}

export const AGENT_TEMPLATES: Record<string, AgentTemplate> = {
  claude: {
    provider: {
      command: "claude",
      args: ["--print", "--output-format", "stream-json", "--verbose", "--dangerously-skip-permissions", "--include-partial-messages"],
      normalizer: "Claude",
      output_format: "StreamJson",
      input_mode: "Arg",
      system_prompt_arg: "--append-system-prompt",
      model_arg: "--model",
      session_arg: null,
      resume_args: [],
      session_id_fields: [],
      clear_env: false,
      no_output_timeout_ms: 30000,
    },
    timeout_seconds: 30000,
    max_instances: 1,
  },
  cursor: {
    provider: {
      command: "cursor-agent",
      args: ["--print", "--output-format", "stream-json", "--force", "--approve-mcps", "--trust", "--stream-partial-output"],
      normalizer: "cursor-agent",
      output_format: "StreamJson",
      input_mode: "Arg",
      system_prompt_arg: null,
      model_arg: "--model",
      session_arg: null,
      resume_args: [],
      session_id_fields: [],
      clear_env: false,
      no_output_timeout_ms: 30000,
    },
    timeout_seconds: 30000,
    max_instances: 1,
  },
  codex: {
    provider: {
      command: "codex",
      args: ["exec", "--json", "--sandbox", "workspace-write", "--skip-git-repo-check"],
      normalizer: "codex",
      output_format: "StreamJsonl",
      input_mode: "Arg",
      system_prompt_arg: null,
      model_arg: "--model",
      session_arg: null,
      resume_args: [],
      session_id_fields: ["thread_id"],
      clear_env: false,
      no_output_timeout_ms: 30000,
    },
    timeout_seconds: 30000,
    max_instances: 1,
  },
  agy: {
    provider: {
      command: "agy",
      args: ["--dangerously-skip-permissions", "--output-format", "stream-json"],
      normalizer: "agy",
      output_format: "StreamJson",
      input_mode: "Arg",
      system_prompt_arg: null,
      model_arg: "--model",
      session_arg: null,
      resume_args: [],
      session_id_fields: ["conversation_id"],
      clear_env: false,
      no_output_timeout_ms: 30000,
    },
    timeout_seconds: 30000,
    max_instances: 1,
  },
};
