// ---------------------------------------------------------------------------
// Shared TypeScript interfaces matching the ao-server backend API contracts.
// ---------------------------------------------------------------------------

/** Identifies whether a tasklist belongs to a team or a project scope.
 *  Used as the stable routing key in the tasklist data layer. */
export type TasklistScope = { kind: "team" | "project"; id: string };

/** Stable string key for a TasklistScope, used as a Map key. */
export function scopeKey(scope: TasklistScope): string {
  return `${scope.kind}:${scope.id}`;
}

/** One agent-visible form waiting on an answer, scoped to the thread it was
 *  posted on — async (`AskUserQuestionWithForm` mode="async") OR sync
 *  (mode="sync", persisted purely for UI reconstruction after a reload).
 *  Mirrors the backend `ao_persistence::snapshot::PendingForm`
 *  shape. `thread_id: null` means the agent's default thread — same
 *  convention as `AgentSnapshot.thread_id` and SSE event `thread_id` tags. */
export interface PendingForm {
  thread_id: string | null;
  form_id: string;
  /** `null` only transiently, on the optimistic entry `useSSE`'s `form_posted`
   *  handler inserts before the follow-up `fetchAgents()` refetch lands the
   *  server's real value — the backend always sends a populated spec. */
  spec: import("./form").PendingFormRequestMeta | null;
  /** `true` once the backend's startup sync-form reaper has determined this
   *  form's owning run/session did not survive a process restart. Only ever
   *  set on `spec.mode === "sync"` entries — a synchronous
   *  `AskUserQuestionWithForm` call cannot resume across a restart (its
   *  `tokio::sync::oneshot` and the parked task both die with the old
   *  process), so this is a durable "nothing will ever answer this" fact.
   *  Async forms never get this set. `undefined`/`false` both mean "not
   *  orphaned" — treat them the same. */
  orphaned?: boolean;
}

/** Lightweight agent summary returned by GET /agents. */
export interface AgentSnapshot {
  agent_id: string;
  name: string;
  emoji?: string;
  last_activity_at: string | null;
  last_agent_activity_at?: string | null;
  last_message?: string;
  message_count: number;
  has_active_run: boolean;
  /** Thread ids among this agent's own threads that currently have an active
   *  run — connection-independent REST-polled counterpart to `has_active_run`,
   *  scoped down to the specific thread instead of the whole agent. Optional
   *  for back-compat with snapshots taken before this field existed. */
  running_thread_ids?: string[];
  queue_depth: number;
  /** Which thread `last_message` landed in — `null` means the agent's
   *  default thread, a concrete id means a fresh/branch thread. Written
   *  alongside `last_message` server-side so the pair always describes the
   *  same event (backend field name is `last_message_thread_id`; the wire
   *  key stays `thread_id` for back-compat with legacy snapshots and every
   *  fixture already typed against this field). Drives ChatSidebar's "jump
   *  to the thread with the last message" click — resolve with
   *  `?? \`default-${agent_id}\`` the same way `lib/threadNavigation.ts`'s
   *  `selectedThreadIdFor` does. */
  thread_id: string | null;
  created_at: string;
  file_capabilities_supported?: boolean;
  /** When set, this agent is an inline team coordinator and is hidden from
   *  chat surfaces by default. Mirrors the backend AgentSnapshot field. */
  owning_team_id?: string | null;
  /** Coordinator level computed at read time on the server. 0 = leaf agent
   *  (no delegation), N = deepest delegation chain of length N. Omitted for
   *  leaf agents (equivalent to 0). */
  coordinator_level?: number;
  /** AskUserQuestionWithForm calls outstanding on this agent, one per thread
   *  that has one pending — at most one entry per distinct `thread_id`
   *  (including at most one with `thread_id: null`, the default thread).
   *  Contains BOTH async (`spec.mode === "async"`) and sync
   *  (`spec.mode === "sync"`) entries — sync ones exist purely so
   *  `hydratePendingSyncFormsFromAgents` can rebuild `pendingFormByAgent`
   *  after a reload; every OTHER reader of this field wants async only, so
   *  use `pendingFormForThread` (`stores/chatStore.ts`), which filters sync
   *  entries out, rather than reading this array directly. */
  pending_forms?: PendingForm[];
  /** Title of the agent's currently-active tasklist. Set while the tasklist
   *  is in `active` status, cleared on terminal transitions. Drives the
   *  sidebar ping indicator. */
  active_tasklist_title?: string | null;
}

/** Full agent profile returned by GET /agents/{id}. */
/** Per-agent enablement state for a single globally-installed plugin. */
export interface PluginEnablement {
  enabled: boolean;
  /** `null`/omitted = every skill the plugin ships is enabled.
   *  Non-null list = only the listed bare skill names are enabled. */
  enabled_skills?: string[] | null;
}

export type AgentRunnerMode = "cli" | "api";

/** Selects which native (in-process API) provider client to instantiate.
 *  Only relevant when `runner_mode === "api"`. Absent → defaults to Anthropic
 *  on the server side for backwards compatibility. Mirrors the Rust
 *  `ao_protocol::agent::NativeProvider` enum (`#[serde(rename_all =
 *  "lowercase")]`) field-for-field — keep the two in lockstep. */
export type AgentNativeProvider = "anthropic" | "openai" | "openrouter";

/** A small ordinal set of reasoning-effort levels for the native (API)
 *  provider path — deliberately not a raw token count. Mirrors the Rust
 *  `ao_protocol::agent::ReasoningEffort` enum (`#[serde(rename_all =
 *  "snake_case")]`) field-for-field. Each provider's request builder maps a
 *  level onto its own wire shape: an extended-thinking token budget for
 *  Anthropic, the native `reasoning_effort` string for OpenAI-compatible
 *  chat completions. */
export type AgentReasoningEffort = "low" | "medium" | "high";

/** Masked view of one provider's entry in `providers.toml`, as returned by
 *  `GET /providers`. Never carries the API key itself — `has_api_key` is a
 *  presence check only, so the frontend can show a "configured" indicator
 *  without the secret ever round-tripping into browser memory. Mirrors the
 *  Rust `ao_engine_tools_provider_config::ProviderStatus` struct
 *  field-for-field. */
export interface ProviderStatus {
  /** `"anthropic" | "openai" | "openrouter" | "gemini"` — kept as `string`
   *  here since this view covers every credential `providers.toml` can
   *  hold, a superset of the {@link AgentNativeProvider} the agent runner
   *  currently supports. */
  provider: string;
  has_api_key: boolean;
  /** Masked fingerprint of the stored key (e.g. `sk-ant-api03…wxyz`), or
   *  `null`/absent when no key is stored or the stored key is too short to
   *  fingerprint safely. Never enough to reconstruct the key. */
  api_key_fingerprint?: string | null;
  base_url: string | null;
  model: string | null;
  /** Persisted `max_output_tokens` default for this provider, if set. */
  max_output_tokens: number | null;
  /** Persisted `max_context_tokens` default for this provider, if set. */
  max_context_tokens: number | null;
  /** Persisted `reasoning_effort` default for this provider, if set. */
  reasoning_effort: AgentReasoningEffort | null;
}

/** Body for `PUT /providers/{name}` — mirrors the Rust
 *  `SetProviderKeyRequest` in `crates/ao-server/src/routes/providers.rs`
 *  field-for-field. `api_key` is mandatory on every call (the route
 *  rejects an empty string with a 400); every other field is an optional
 *  override merged into the existing `providers.toml` section — omitted
 *  (not just empty-string/null) means "leave whatever's already stored". */
export interface SetProviderRequest {
  api_key: string;
  base_url?: string | null;
  model?: string | null;
  max_output_tokens?: number | null;
  max_context_tokens?: number | null;
  reasoning_effort?: AgentReasoningEffort | null;
}

/** The `code` field `GET /providers/{name}/models` attaches to its three
 *  distinguishable upstream-outcome error classes. Mirrors the three
 *  `AoError::Provider{AuthFailure,NetworkFailure,MalformedResponse}`
 *  variants' `code` strings from `crates/ao-server/src/error.rs`. Absent on
 *  the plain 400/500 precondition failures (unknown provider, no stored
 *  key, corrupt `providers.toml`) that never reach the network. */
export type ProviderModelDiscoveryErrorCode = "auth_failure" | "network_failure" | "malformed_response";

/** Provider-neutral reasoning channel posture.
 *
 *  - `adaptive` lets the provider decide how much reasoning to engage on a
 *    per-prompt basis (default).
 *  - `disabled` opts the agent out of extended thinking entirely; the
 *    request body omits the `thinking` field on the Anthropic API path.
 */
export type AgentThinkingMode = "adaptive" | "disabled";

/** How much of the model's reasoning to surface to the client.
 *
 *  - `summarized` — provider returns a digested summary of the reasoning.
 *    Right tradeoff for chat UIs.
 *  - `raw` — full reasoning text, character-for-character. Useful for
 *    debugging or transparency-heavy UIs.
 *  - `omitted` — no reasoning text on the wire; only the cryptographic
 *    signature proving thinking occurred. Mounts a "Thinking…" indicator
 *    without progressive deltas.
 */
export type AgentThinkingDisplay = "summarized" | "raw" | "omitted";

/** Per-agent reasoning channel configuration. Persisted on the agent
 *  profile so the same settings drive a CLI runner today and the API
 *  runner tomorrow without translating provider-specific flag names.
 *
 *  `budget_tokens` is an optional hard cap on reasoning-channel tokens
 *  for a single turn. `null`/omitted lets the provider apply its default
 *  (typically a function of model + mode). */
export interface AgentThinkingConfig {
  mode: AgentThinkingMode;
  display: AgentThinkingDisplay;
  budget_tokens?: number | null;
}

/** A single entry in an agent's delegate address book. Mirrors the Rust
 *  `DelegateTarget` struct in ao-protocol. */
export interface DelegateTarget {
  /** AgentProfile.id of the target agent. */
  target_agent_id: string;
  /** Local alias for the target (defaults to target agent's name). */
  name: string;
  /** Human-readable description of what this agent handles. */
  purpose: string;
  /** Whether the delegating agent may share its context with this target. */
  share_context_allowed: boolean;
}

export type TelegramThreadMode = "dedicated";

export interface TelegramConfig {
  enabled: boolean;
  /** Cached bot display name (e.g. "axew_research_bot") from a `getMe` call. */
  bot_username?: string | null;
  thread_mode: TelegramThreadMode;
  /** Server-owned id of the dedicated thread all Telegram traffic for this agent
   *  flows through. Provisioned on first enable; a client-supplied value is never
   *  trusted over the one already on record. */
  bridge_thread_id?: string | null;
  /** Chat IDs linked to this agent via the pairing flow. Empty = unlinked. */
  allowed_chat_ids: number[];
}

export interface AgentProfile {
  id: string;
  name: string;
  description: string;
  emoji?: string;
  provider: ProviderConfig;
  model: string | null;
  skills: string[];
  system_prompt: string | null;
  tools: ToolsConfig | null;
  env: Record<string, string>;
  max_instances: number;
  timeout_seconds: number;
  /** Per-agent cap on model-completion turns for a native (in-process API)
   *  run, sitting alongside `timeout_seconds` as the other safety rail.
   *  `null`/omitted defers to the backend's `DEFAULT_MAX_TURNS`. Relevant
   *  only when `runner_mode === "api"`. */
  max_turns?: number | null;
  working_dir: string | null;
  home_dir: string | null;
  serialize: boolean;
  workflows?: string | string[] | null;
  /** Preset template the agent was created from (e.g. "claude", "cursor", "codex").
   *  `null`/omitted means a fully custom configuration. */
  template?: string | null;
  /** Which runner drives this agent. Defaults to "cli" for legacy profiles.
   *  Locked at creation — cannot be changed on existing agents. */
  runner_mode?: AgentRunnerMode;
  /** Picks which API provider client `NativeAgentRunner` builds when
   *  `runner_mode === "api"`. Ignored on CLI-mode agents. Absent → server
   *  defaults to Anthropic. */
  native_provider?: AgentNativeProvider;
  /** Reasoning channel posture for this agent. `null`/omitted falls back
   *  to the provider's default (no extended thinking on the API path; the
   *  `display = "omitted"` adaptive shape on the CLI path). */
  thinking?: AgentThinkingConfig | null;
  /** Per-agent override for the native (API) provider's response-length
   *  cap. Resolved with the same per-agent ?? persisted-`providers.toml`
   *  ?? provider-default precedence as `model`. `null`/omitted defers to
   *  the next tier. */
  max_output_tokens?: number | null;
  /** Per-agent override for the approximate conversation-history budget
   *  (in tokens) the native runner keeps in context before trimming the
   *  oldest turns. Same precedence as `max_output_tokens`. `null`/omitted
   *  means no cap enforced. */
  max_context_tokens?: number | null;
  /** Per-agent override for the native (API) reasoning-effort level. Same
   *  precedence as `max_output_tokens`. Distinct from `thinking` above —
   *  see the Rust `ReasoningEffort` doc comment for how the two relate. */
  reasoning_effort?: AgentReasoningEffort | null;
  /** Per-agent enablement map for globally-installed plugins, keyed by plugin name.
   *  Omitted from the server payload when empty. */
  enabled_plugins?: Record<string, PluginEnablement>;
  /** Names of enabled convention-folder skills from the global
   *  `<data_root>/.launchpad/skills` directory. Explicit opt-in — absent/empty
   *  means none enabled. Omitted from the server payload when empty. */
  enabled_launchpad_global_skills?: string[];
  /** Per-project enablement of convention-folder skills from
   *  `<focus_path>/.launchpad/skills`, keyed by canonical project key. Same
   *  explicit opt-in semantics as `enabled_launchpad_global_skills`. Omitted
   *  from the server payload when empty. */
  enabled_launchpad_project_skills?: Record<string, string[]>;
  /** When set, the agent is an inline team coordinator and is hidden from
   *  chat surfaces by default. Mirrors the backend AgentProfile field. */
  owning_team_id?: string | null;
  /** Address book for the Delegate tool. Omitted when empty (pre-H profiles). */
  delegates_to?: DelegateTarget[];
  /** Identity, voice, and expertise authored by the agent creator. Null when unset — the
   *  composer then falls back to deriving persona content from the legacy `system_prompt`
   *  field at runtime (see `resolve_persona_fields` in the backend composer). */
  persona?: string | null;
  /** Behavior rules and project-specific guidelines authored by the agent creator. */
  special_instructions?: string | null;
  /** Archival copy of persona/special_instructions (or the legacy system_prompt) prior to
   *  the most recent AgentAuthor-driven update — a one-step undo, not a migration artifact. */
  legacy_system_prompt?: string | null;
  /** Telegram bridge configuration. Input-only legacy shape — the server
   *  folds this into `channels` on deserialize (see `AgentProfileWire` in
   *  `crates/ao-protocol/src/agent.rs`) and never re-emits it on output, so a
   *  profile fetched from the API never has this field populated. Kept here
   *  only because some request bodies still accept it. */
  telegram?: TelegramConfig | null;
  /** This agent's bound messaging channels (Telegram, Discord, Email, ...).
   *  This is the field a fetched profile actually carries — `telegram` above
   *  never appears in server output. Omitted/empty means no channels are
   *  configured. */
  channels?: ChannelBinding[];
}

/** Mirrors the backend `ChannelBinding` struct
 *  (`crates/ao-protocol/src/agent.rs`) — one messaging channel bound to an
 *  agent, e.g. a Telegram bot or a polled email inbox. */
export interface ChannelBinding {
  binding_id: string;
  kind: "telegram" | "discord" | "email" | "slack" | "whatsapp" | "webhook";
  enabled: boolean;
  /** The single thread all inbound/outbound traffic for this binding flows
   *  through. Server-owned; `null`/omitted until enabling has provisioned it. */
  bridge_thread_id?: string | null;
  allowed_senders?: string[];
  kind_config?: Record<string, unknown>;
}

export interface FileCapability {
  supported: boolean;
  max_file_size_bytes: number;
  max_attachments_per_message: number;
  allowed_mime_types: string[];
  image_mode: { type: "file_reference"; instruction_template: string };
}

export interface ProviderConfig {
  type: string;
  command: string;
  args: string[];
  output_format: string;
  input_mode: string;
  normalizer?: string | null;
  model_arg?: string | null;
  model_aliases: Record<string, string>;
  system_prompt_arg?: string | null;
  session_arg?: string | null;
  resume_args: string[];
  session_id_fields: string[];
  clear_env: boolean;
  no_output_timeout_ms: number;
  file_capabilities?: FileCapability | null;
}

export interface ToolsConfig {
  allow: string[];
  deny: string[];
  require_approval: string[];
}

/**
 * A single entry in an agent's transcript.
 *
 * `role` is an untagged enum in the backend:
 *  - User messages have `role` as a literal string, e.g. `"user"`
 *  - Agent messages have `role` as an object, e.g. `{ agent: "researcher" }`
 */
export interface TranscriptEntry {
  ts: string;
  role: string | { agent: string } | { task_id: string };
  content: string;
  event_type: string;
  metadata?: Record<string, unknown> | null;
  status?: "sending" | "sent" | "delivered" | "seen" | "error";
  /**
   * When true, the entry is a synthetic injection (e.g. a skill-body load)
   * persisted so the agent's next turn sees it, but not rendered in the UI.
   * Backend omits the field when false (wire-compat default), so treat
   * `undefined` as `false`.
   */
  hidden_from_user?: boolean;
}

/** ACK returned by POST /agents/{id}/messages. */
export interface MessageAck {
  message_id: string;
  status: string;
}

/** A single bookmark entry for a saved message. */
export interface BookmarkEntry {
  id: string;
  agent_id: string;
  message_ts: string;
  message_content: string;
  message_role: string | { agent: string };
  created_at: string;
}

/** A single memory entry stored for an agent or globally. */
export type MemoryScope = "Agent" | "Project" | "Global" | "AgentProject" | "Thread";

export interface MemoryEntry {
  id: string;
  content: string;
  created_at: string;
  /** `null` for project-scoped entries, which carry no authoring source. */
  source: "Manual" | "Agent" | "GlobalPromotion" | null;
  /** Storage scope. May be absent on legacy agent/global payloads. */
  scope?: MemoryScope;
  /** Set by the review queue's `pin` action: exempt from the eviction sweep. */
  pinned?: boolean;
}

/** Kind tag a staged candidate carries, shared with outcome tracking. */
export type ReflectionCandidateKind = "memory" | "skill";

/** Review state of a staged self-improvement candidate. */
export type ReflectionCandidateStatus =
  | "pending"
  | "confirmed"
  | "rejected"
  | "distilled";

/**
 * A memory or skill the reflection pass proposed from a transcript, sitting in
 * the review queue until a human keeps / edits / pins / forgets it. Nothing
 * here is live yet. Mirrors the backend `ReflectionCandidate`
 * (crates/ao-protocol/src/reflection_candidate.rs).
 */
export interface ReflectionCandidate {
  id: string;
  kind: ReflectionCandidateKind;
  agent_id: string;
  source_thread_id: string;
  content: string;
  status: ReflectionCandidateStatus;
  /** Scope this lands in once approved. Absent on rows written before this
   *  field existed (read as "Agent"). */
  target_scope?: MemoryScope;
  target_scope_key?: string | null;
  /** Id of an existing entry this candidate may restate or contradict, if any. */
  contradicts?: string | null;
  reason: string;
  created_at: string;
}

// ---------------------------------------------------------------------------
// Team types
// ---------------------------------------------------------------------------

/** A single team member with their role. */
export interface TeamMember {
  agent_id: string;
  role_description: string;
  working_dir?: string | null;
}

// ---------------------------------------------------------------------------
// Pagination types
// ---------------------------------------------------------------------------

/** Which transcript file a {@link PaginationCursor}'s `byte_offset` addresses.
 *  `"own"` (the default — absent on older cursors) points into the requested
 *  thread's own transcript. `"inherited"` means pagination has walked off the
 *  start of a branch thread's own file and now addresses its SOURCE thread's
 *  transcript instead; round-trip it back as `cursor_phase` on the next
 *  "load older" call so the server keeps reading from the right file. */
export type CursorPhase = "own" | "inherited";

/** Cursor for byte-offset based pagination of transcript messages. */
export interface PaginationCursor {
  byte_offset: number;
  last_message_id: string;
  timestamp: string;
  phase?: CursorPhase;
}

/** Paginated response wrapping messages with an optional cursor for load-more. */
export interface PaginatedMessagesResponse {
  messages: TranscriptEntry[];
  cursor: PaginationCursor | null;
}

/** Project-specific messages response — includes pending async form state for
 *  reload durability. The form fields are absent when no form is outstanding. */
export interface ProjectMessagesResponse extends PaginatedMessagesResponse {
  pending_form_id?: string | null;
  pending_form_spec?: import("./form").AsyncFormRequestMeta | null;
}

// ---------------------------------------------------------------------------
// Cache types
// ---------------------------------------------------------------------------

/** A cached set of messages for a single conversation. */
export interface CacheEntry {
  allMessages: TranscriptEntry[];
  displayCount: number;
  lastAccessed: number;
  cursor: PaginationCursor | null;
  profile?: AgentProfile | null;
}

// ---------------------------------------------------------------------------
// Attachment types
// ---------------------------------------------------------------------------

export type AttachmentType = "image" | "document" | "spreadsheet" | "code" | "folder" | "other";

export interface Attachment {
  id: string;
  file_path: string;
  mime_type: string;
  original_filename: string;
  size_bytes: number;
  attachment_type: AttachmentType;
}

export interface PendingAttachment {
  id: string;
  file: File | null;
  previewUrl: string | null;
  /** "expired" means the attachment was successfully uploaded earlier but the
   *  server-side asset has since been cleaned up (e.g. by the 1-hour
   *  uncommitted-asset GC). The entry is kept in the UI to notify the user
   *  but is excluded from message sends. */
  status: "pending" | "uploading" | "uploaded" | "error" | "expired";
  serverId: string | null;
  attachment: Attachment | null;
  isFolder: boolean;
  /** Full folder path for folder references (used for tooltip). */
  folderPath?: string;
}

// ---------------------------------------------------------------------------
// Artifact types
// ---------------------------------------------------------------------------

/** Which renderer draws an artifact's payload. `"unknown"` is the
 *  forward-compat fallback for a kind string this build doesn't recognize. */
export type ArtifactKind =
  | "list"
  | "cards"
  | "table"
  | "board"
  | "metric"
  | "chart"
  | "html"
  | "unknown";

/** How to read the payload blob, independent of `ArtifactKind`. */
export type PayloadFormat = "json" | "html";

/** Whether and how an artifact updates after it is first rendered.
 *  `"brokered"` is reserved for the planned in-artifact bridge and is never
 *  emitted in v1. `"unknown"` is the forward-compat fallback. */
export type RefreshIntent = "none" | "whole_artifact" | "brokered" | "unknown";

/** One allowlisted data slice an artifact may request from the planned
 *  in-artifact bridge. Declared at author time; not served until then. */
export interface CapabilitySpec {
  slice: string;
  params_schema: unknown;
}

/** The replayable authoring request that produced an artifact, replayed on a
 *  whole-artifact refresh. */
export interface OriginIntent {
  refresh_prompt: string;
}

/** Which surface performed a mutation recorded in an artifact's
 *  `intent_ledger`. `"unknown"` is the forward-compat fallback for a source
 *  string this build doesn't recognize. Mirrors `IntentSource`
 *  (`crates/ao-protocol/src/artifact.rs`). */
export type IntentSource = "create" | "regenerate" | "main_thread_edit" | "chat" | "unknown";

/** One append-only entry in an artifact's edit history. Mirrors
 *  `IntentLedgerEntry` (`crates/ao-protocol/src/artifact.rs`). */
export interface IntentLedgerEntry {
  timestamp: string;
  source: IntentSource;
  intent_note: string | null;
  source_message_id: string | null;
}

/** A renderable output an agent produced — a typed dataset or freeform HTML —
 *  persisted so it can be displayed, refreshed in place, and reopened later.
 *  Mirrors `ArtifactRecord` (`crates/ao-protocol/src/artifact.rs`). */
export interface Artifact {
  id: string;
  title: string;
  kind: ArtifactKind;
  format: PayloadFormat;
  stored_filename: string;
  size_bytes: number;
  checksum_sha256: string;
  refresh_intent: RefreshIntent;
  origin_intent: OriginIntent | null;
  capabilities: CapabilitySpec[];
  source_message_id: string | null;
  created_at: string;
  updated_at: string;
  last_refreshed_at: string | null;
  refresh_count: number;
  /** Set by the Assets page's pin action; drives the cross-agent pinned
   *  listing. Purely a display flag — artifacts have no eviction sweep. */
  pinned: boolean;
  /** When this artifact was last pinned, or `null` if never pinned/currently
   *  unpinned. Drives the Assets sidebar's newest-pinned-first ordering. */
  pinned_at: string | null;
  /** Id of the `ArtifactGroup` this artifact is filed under in the Assets
   *  sidebar, or `null` for the ungrouped list. */
  group_id: string | null;
  /** Whether a prior edit exists to revert to via the undo endpoint. Drives
   *  the header Undo button's enabled state — the server is the source of
   *  truth for how much edit history is retained, so this flips back to
   *  `false` once that history is exhausted rather than the client tracking
   *  a local count. Optional here (unlike the backend record, which always
   *  populates it) so existing fixtures/mocks that predate this field don't
   *  need updating — readers should treat a missing value as `false`. */
  undo_available?: boolean;
  /** Append-only history of why this artifact was written or rewritten,
   *  oldest first. Optional here (unlike the backend record, which always
   *  populates it) so existing fixtures/mocks that predate this field don't
   *  need updating — readers should treat a missing value as empty. The chat
   *  mini-thread panel reads the last entry's `intent_note` as the agent's
   *  reply once a chat-triggered adjustment lands (see `ArtifactChatPanel`). */
  intent_ledger?: IntentLedgerEntry[];
}

/** A user-defined bucket for organizing pinned artifacts in the Assets
 *  sidebar, rendered as a collapsible section. Mirrors `ArtifactGroup`
 *  (`crates/ao-protocol/src/artifact.rs`). */
export interface ArtifactGroup {
  id: string;
  name: string;
  created_at: string;
}

/** An artifact's metadata alongside its current payload — what the `get`
 *  endpoint returns, since the record alone isn't enough to render anything.
 *  `payload` is a JSON value for typed kinds or a JSON string for `"html"`. */
export interface ArtifactWithPayload extends Artifact {
  payload: unknown;
  /** Id of a background task still `Running` against this artifact (a
   *  regenerate or chat-to-adjust run), or `null`/absent if none is in
   *  flight. Populated only on this `get` response — that's the fetch
   *  `ArtifactPreview` already makes on every mount, so a caller that finds
   *  this non-null while `useArtifactRegen` is otherwise idle knows to call
   *  its `resume()` and restore the "Adjusting…" spinner instead of leaving
   *  a still-running task with no feedback on screen. Optional (not just
   *  nullable) so existing fixtures/mocks that predate this field don't
   *  need updating — readers should treat a missing value the same as
   *  `null`. */
  running_task_id?: string | null;
}

/** One entry in the global pinned-artifacts listing (`GET /artifacts/pinned`)
 *  — an `Artifact` plus the id of the agent that owns it, since the record
 *  alone doesn't say which agent produced it. */
export interface PinnedArtifact extends Artifact {
  agent_id: string;
}

/** Body for POST /agents/{agent_id}/artifacts. */
export interface CreateArtifactRequest {
  title: string;
  kind: ArtifactKind;
  format: PayloadFormat;
  payload: unknown;
  refresh_intent?: RefreshIntent;
  origin_intent?: OriginIntent | null;
  capabilities?: CapabilitySpec[];
  source_message_id?: string | null;
}

// ---------------------------------------------------------------------------
// Tasklist types
// ---------------------------------------------------------------------------

export type TaskStatus =
  | "pending"
  | "in_progress"
  | "completed"
  | "failed"
  | "blocked"
  | "skipped"
  /** Per-task halt: explicitly stopped (by the user or the orchestrating
   *  agent) while it was in progress. Non-terminal — resuming flips it back
   *  to "pending" for re-dispatch. */
  | "stopped";

/** Assignment mode for a classifier-routed or pinned task. */
export type AssignmentMode = "pinned" | "classified";

/** Task assignment carrying the owning agent and how ownership was determined. */
export interface TaskAssignment {
  owner_agent_id: string;
  mode: AssignmentMode;
}

export type TaskGroupMode = "PAR" | "SEQ";

export type TasklistStatus =
  | "active"
  | "paused"
  | "completed"
  | "cancelled"
  | "failed";

export type TaskCommentAuthorKind = "user" | "agent";

/** A comment attached to a single task. Augments the task prompt at
 *  dispatch time but never overrides it. */
export interface TaskComment {
  id: string;
  author_id: string;
  author_kind: TaskCommentAuthorKind;
  body: string;
  created_at: string;
}

export interface Task {
  id: string;
  owner_agent_id: string;
  prompt: string;
  /** Classifier-routed or pinned assignment. Null means awaiting classification. */
  assignment?: TaskAssignment | null;
  /** Optional long-form description distinct from `prompt`. When unset the
   *  modal omits the Description section entirely (no fallback to prompt). */
  description?: string;
  expected_outputs: string[];
  status: TaskStatus;
  group_id: string;
  attempt_count: number;
  error_log: string[];
  /** Inline list of comments attached to this task. Defaults to `[]` for
   *  legacy serialized tasks. */
  comments?: TaskComment[];
  /** Team-scoped attachments bound to the task at append time. The classifier
   *  sees the filenames in its routing prompt; the executing agent sees the
   *  full attachment block via the runtime augmenter. Defaults to `[]` for
   *  legacy serialized tasks. */
  attachments?: Attachment[];
  /** Agent that should be notified via `<task-item-notification>` when this
   *  task reaches a terminal state. `null`/absent means no notification
   *  routing. Snake_case to match the Rust wire format. */
  remind_me?: string | null;
  /** True when the auto-reprompt retry budget for the
   *  `<task-item-notification>` block was exhausted and a synthesized
   *  changelog entry was written. Absent on legacy serialized tasks. */
  parse_failed?: boolean;
}

export interface TaskGroup {
  id: string;
  mode: TaskGroupMode;
  tasks: Task[];
}

export interface Tasklist {
  id: string;
  team_id: string;
  title: string;
  description: string;
  status: TasklistStatus;
  groups: TaskGroup[];
  workspace_dir: string;
  transcripts_dir: string;
  created_at: string;
  last_active_at?: string | null;
  /** Set when this tasklist was created by a project agent. Drives the
   *  agent-channel SSE filter: personal-chat handlers skip events with a
   *  non-null project_id so project work never bleeds into the agent store. */
  project_id?: string | null;
}

/** Resolved owner agent reference for {@link TaskDetail}. `null` is used at
 *  the parent level when a task has no owner; this type itself is always a
 *  populated reference. `emoji` is the agent's avatar glyph when set. */
export interface TaskDetailAssignedAgent {
  id: string;
  name: string;
  emoji?: string;
}

/** Full task detail surfaced in the TeamsView task detail modal. Extends the
 *  wire-format {@link Task} with derived/resolved fields the modal needs:
 *   - `title`: first non-empty line of the task prompt (matches the inline
 *     tasklist row's display heuristic).
 *   - `description`: pass-through of {@link Task.description}. Optional —
 *     the modal renders the Description section only when this is a
 *     non-empty string; there is no fallback to `prompt`.
 *   - `assigned_agent`: resolved from `owner_agent_id`. `null` when the task
 *     is unassigned (`owner_agent_id === ""`) or the agent can't be resolved.
 *  Comments and attachments come from {@link Task} unchanged; consumers
 *  should treat the optional arrays as `?? []`. */
export interface TaskDetail extends Task {
  title: string;
  assigned_agent: TaskDetailAssignedAgent | null;
}

export interface ListTasklistsResponse {
  active: Tasklist | null;
  recent: Tasklist[];
}

export interface CreateTasklistRequest {
  title: string;
  description?: string;
  groups: CreateTasklistGroupRequest[];
  /** Opt-in for user-driven empty-shell tasklists. When true and
   *  `groups` is empty, the backend skips the empty-groups validation and
   *  persists a tasklist in `Paused` status. Agent-emitted creates leave
   *  this unset so the strict empty-groups check still applies. */
  allow_empty_groups?: boolean;
}

export interface CreateTasklistGroupRequest {
  mode: TaskGroupMode;
  tasks?: CreateTasklistTaskRequest[];
}

export interface CreateTasklistTaskRequest {
  owner_agent_id: string;
  prompt: string;
  expected_outputs?: string[];
}

/** Body for POST /teams/{team_id}/tasklists/{tasklist_id}/tasks.
 *  Mode-routing on the server: if the tasklist's last group mode matches
 *  `mode`, the task is appended to that group; otherwise a new group is
 *  created. `owner_agent_id` may be omitted/null to trigger coordinator
 *  routing. */
export interface AppendTaskRequest {
  prompt: string;
  mode: TaskGroupMode;
  owner_agent_id?: string;
  expected_outputs?: string[];
  /** IDs of team-scoped attachments to bind to the task. The server resolves
   *  each ID against the team's attachment store; unknown IDs are dropped. */
  attachment_ids?: string[];
}

/** Body for POST /teams/{team_id}/tasklists/{tasklist_id}/tasks/{task_id}/comments.
 *  `body` is the comment text. `author_kind` and `author_id` are optional in
 *  v1 — when omitted, the server defaults to author_kind=user with
 *  author_id="user". */
export interface AddTaskCommentRequest {
  body: string;
  author_kind?: TaskCommentAuthorKind;
  author_id?: string;
}

// ---------------------------------------------------------------------------
// Project types
// ---------------------------------------------------------------------------

export type ProjectStatus = "interviewing" | "active" | "completed" | "archived" | "needs_review";

export interface Project {
  id: string;
  name: string;
  emoji?: string | null;
  goal: string;
  spec?: string | null;
  agent_id: string;
  working_dir?: string | null;
  attachments: string[];
  status: ProjectStatus;
  summary?: string | null;
  created_at: string;
  updated_at: string;
}

/** Lightweight project summary returned by GET /projects. */
export interface ProjectListItem {
  id: string;
  name: string;
  emoji?: string | null;
  status: ProjectStatus;
  agent_id: string;
  created_at: string;
  updated_at: string;
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/** Returns true if the transcript entry is a user message. */
export function isUserMessage(entry: TranscriptEntry): boolean {
  return typeof entry.role === "string" && entry.role === "user";
}

/** Returns true if the transcript entry is an agent message. */
export function isAgentMessage(entry: TranscriptEntry): boolean {
  return typeof entry.role === "object" && entry.role !== null && "agent" in entry.role;
}

/** Returns true if the transcript entry is a scheduled task trigger. */
export function isScheduleMessage(entry: TranscriptEntry): boolean {
  return typeof entry.role === "object" && entry.role !== null && "task_id" in entry.role;
}

// ---------------------------------------------------------------------------
// TodoList event payload types (batched return)
// ---------------------------------------------------------------------------

/** A single task entry in a TodoListCreated event. */
export interface TodoListCreatedItem {
  task_id: string;
  title: string;
  assignment?: TaskAssignment | null;
}

/** Terminal outcome counts within a TodoListComplete event. */
export interface TodoListTerminalCounts {
  succeeded: number;
  failed: number;
  skipped: number;
  cancelled: number;
}

/** Per-task terminal outcome within a TodoListComplete event. */
export interface TodoListCompleteTask {
  task_id: string;
  title: string;
  status: string;
  summary?: string | null;
  owner_agent_id?: string | null;
}

// ---------------------------------------------------------------------------
// DelegateStarted / DelegateComplete event payloads (async delegate lifecycle)
// ---------------------------------------------------------------------------

/** Payload for the `delegate.started` SSE event — fired once, the instant an
 *  async delegate's background run is registered. Brackets with
 *  `DelegateCompletePayload` below. */
export interface DelegateStartedPayload {
  delegate_name: string;
  delegation_id: string;
  /** ISO 8601 UTC timestamp of when the delegate was actually spawned.
   *  Carried through on reconnect replay so an elapsed-time indicator can
   *  pick up where it would have been had the connection never dropped,
   *  instead of restarting the clock at the replay time. */
  spawned_at: string;
}

/** Payload for the `delegate.complete` SSE event. */
export interface DelegateCompletePayload {
  delegate_name: string;
  delegation_id: string;
  /** "completed" | "failed" | "cancelled" */
  status: string;
  duration_ms?: number | null;
  transcript_path: string;
}

// ---------------------------------------------------------------------------
// Thread types
// ---------------------------------------------------------------------------

export type ThreadKind = "default" | "fresh" | "branch";

export interface BranchSource {
  source_thread_id: string;
  /** ISO 8601 UTC timestamp at which the branch inherits history. */
  branch_at: string;
  source_message_id?: string | null;
}

/** Which channel binding created this thread as its dedicated bridge
 *  conversation — mirrors the backend `ChannelBridgeOrigin`
 *  (`crates/ao-protocol/src/thread.rs`). See `lib/threadNavigation.ts`'s
 *  `getBridgeChannelKind`, which checks this in addition to
 *  `ChannelBinding.bridge_thread_id`: a channel that provisions one thread
 *  per *conversation* instead of one per binding (Slack) never populates
 *  `bridge_thread_id` at all, so this field is the only way to recognize
 *  one of its bridge threads. */
export interface ChannelBridgeOrigin {
  kind: ChannelBinding["kind"];
  binding_id: string;
}

/** Which assignment created this thread as a run's own conversation —
 *  mirrors the backend `AssignmentBridgeOrigin`
 *  (`crates/ao-protocol/src/thread.rs`). Set only for `fresh`- and
 *  `dedicated`-policy assignment runs; never set for `main`-policy runs,
 *  which land in the agent's ordinary default thread alongside interactive
 *  chat. `run_id` names the one run that created a `fresh` thread; it's
 *  absent for a `dedicated` thread, which persists across many runs. */
export interface AssignmentBridgeOrigin {
  assignment_id: AssignmentId;
  run_id?: string | null;
}

export interface Thread {
  id: string;
  title: string | null;
  /** System-derived label shown while `title` is unset (see `title ?? auto_title ?? fallback`). */
  auto_title?: string | null;
  scope:
    | { type: "AgentChat"; agent_id: string }
    | { type: "TeamChat"; team_id: string }
    | { type: "Delegation"; team_id: string; delegation_id: string };
  transcript_path: string;
  kind: ThreadKind;
  history_floor_ts?: string | null;
  branch_source?: BranchSource | null;
  /** Set when the thread has been archived — hidden from the tab strip, the
   *  overflow panel, `ThreadsPanel`'s main list, and Home's thread list until
   *  unarchived. `null`/absent means visible everywhere. */
  archived_at?: string | null;
  /** Set when this thread was created as a channel binding's dedicated
   *  bridge conversation. See `ChannelBridgeOrigin`. */
  channel_origin?: ChannelBridgeOrigin | null;
  /** Set when this thread was created as an assignment's own run
   *  conversation. See `AssignmentBridgeOrigin`. */
  assignment_origin?: AssignmentBridgeOrigin | null;
  created_at: string;
  updated_at: string;
}

// ---------------------------------------------------------------------------
// Assignment types
// ---------------------------------------------------------------------------

export type AssignmentId = string;

/** What a `ConnectorEvent` trigger polls on each tick, and how the poll loop
 *  extracts a comparable value for cursor dedup from the result. Mirrors
 *  `ConnectorPollSpec` on the backend. */
export interface ConnectorPollSpec {
  /** MCP tool to invoke each poll (e.g. `"list_emails"`, `"search_issues"`). */
  tool_name: string;
  /** Arguments passed as the tool call's `arguments` object. */
  arguments: unknown;
  /** Dot-path into the raw `tools/call` result identifying the value to
   *  compare against the assignment's last observed cursor. `null`/absent
   *  falls back to comparing the entire serialized result. */
  cursor_path?: string | null;
}

/** A single field-level webhook filter test: read `field` (a dot-path into
 *  the inbound payload, e.g. `"pull_request.title"`) and evaluate `op`
 *  against whatever value (if any) is found there. Mirrors
 *  `WebhookFieldFilter`/`WebhookFilterOp` on the backend — `op`'s extra
 *  fields (`value`/`values`/`path`/`pattern`) are flattened onto the same
 *  object as `field`, matching the backend's `#[serde(flatten)]` shape. */
export type WebhookFieldFilter = { field: string } & (
  | { op: "equals"; value: unknown }
  | { op: "not_equals"; value: unknown }
  | { op: "contains"; value: unknown }
  | { op: "in"; values: unknown[] }
  | { op: "in_file"; path: string }
  | { op: "regex"; pattern: string }
  | { op: "exists" }
  | { op: "missing" }
);

/** A declarative filter tree: a single field test, or `all`/`any`/`not`
 *  combinators over subtrees. Untagged on the wire (matches the backend's
 *  `#[serde(untagged)]` `WebhookFilter`) — variants are told apart by which
 *  key is present. */
export type WebhookFilter =
  | { all: WebhookFilter[] }
  | { any: WebhookFilter[] }
  | { not: WebhookFilter }
  | WebhookFieldFilter;

/** Where a `Webhook` delivery that passes `events`/`filters` is routed.
 *  Mirrors `WebhookDeliverTarget` on the backend. */
export type WebhookDeliverTarget =
  | { type: "agent" }
  | { type: "deliver_only" }
  | { type: "github_comment" };

/** What kind of source a watch contract observes, and the source's own
 *  container reference. `kind`/`ref` are free-form strings chosen by the
 *  authoring agent (e.g. a connector kind and a native container id) —
 *  never branched on by name anywhere in the app. Mirrors `WatchSource` on
 *  the backend. */
export interface WatchSource {
  kind: string;
  ref: string;
}

/** The identity fallback ladder a watch contract commits to at authoring
 *  time. Mirrors `IdentityStrategy`
 *  on the backend. */
export type WatchIdentityStrategy = "native_id" | "composite_native" | "content_hash";

/** "Who is this?" — what identity means for one watch, and why. Exactly one
 *  of `source_field` (for `native_id`) or `fields` (for
 *  `composite_native`/`content_hash`) is meaningful, depending on
 *  `strategy`. Mirrors `IdentitySpec` on the backend. */
export interface WatchIdentitySpec {
  strategy: WatchIdentityStrategy;
  /** The field the source itself exposes as a stable per-item key,
   *  discovered by the authoring agent — only meaningful for `native_id`. */
  source_field?: string | null;
  /** Regex the authoring agent derived from the values it actually observed
   *  for `source_field`. A relayed value that fails this at runtime is
   *  quarantined rather than treated as a new item. */
  format?: string | null;
  /** Fields combined to form identity for `composite_native`/`content_hash`;
   *  unused for `native_id`. */
  fields: string[];
  /** The authoring agent's own explanation of why this rung, including the
   *  stability-probe result — the single most trust-relevant sentence on
   *  the contract. */
  rationale: string;
}

/** "Has it changed in a way we care about?" Mirrors `ChangeSpec` on the
 *  backend. */
export interface WatchChangeSpec {
  /** Fields whose change is meaningful enough to count as a version change —
   *  deliberately a strict subset of the contract's declared `fields`. */
  material_fields: string[];
  /** A cheap pre-filter field the runtime tick may use to skip
   *  re-extraction when unchanged; never part of the fire decision itself. */
  version_hint_field?: string | null;
}

/** "Should it fire?" `predicate`
 *  is the typed form the runtime evaluates; `natural_language` is what a
 *  human should be shown. Mirrors `PredicateSpec` on the backend. */
export interface WatchPredicateSpec {
  natural_language: string;
  fields: string[];
  /** The typed fire condition the runtime actually evaluates. Opaque to the
   *  UI — it is a recursive tagged union, and nothing here interprets it.
   *  For display, read `AssignmentWatchHealth.predicate_expr`, which the
   *  backend renders from this value.
   *
   *  This replaced a flat `expr: string`. That field no longer exists on the
   *  wire at all (the backend's `Serialize` is derived and has no such
   *  field), so anything reading `predicate.expr` silently rendered
   *  `undefined`. */
  predicate: unknown;
}

/** How a matching item translates into a fire decision
 *  `predicate_transition` (the v1
 *  default) fires once on a false→true edge, whichever direction produced
 *  it. Mirrors `WatchMode` on the backend. */
export type WatchMode = "predicate_transition" | "new_or_changed" | "new_only";

/** One field's extraction contract: its type, whether a poll missing it
 *  counts as an extraction failure, and which cheap normalizations apply to
 *  it before hashing/comparison. Mirrors `FieldSpec` on the backend. */
export interface WatchFieldSpec {
  type: string;
  required: boolean;
  normalize: string[];
}

/** A watch's frozen declaration of what it observes and what identity,
 *  change, and firing mean for it — authored once by the watch's own agent
 *  on its first run, then injected unmodified into every subsequent poll.
 *  Read-only everywhere in the app today: editing, amendment, and re-keying
 *  are deferred past v1. Mirrors
 *  `WatchContract` on the backend. */
export interface WatchContract {
  contract_version: number;
  /** RFC3339 timestamp of authoring. */
  authored_at: string;
  /** Id of the run that authored (or last amended) this contract. */
  authored_by_run: string;
  source: WatchSource;
  /** Answers "who is this?" */
  identity: WatchIdentitySpec;
  /** Answers "has it changed in a way we care about?" */
  change: WatchChangeSpec;
  /** Answers "should it fire?" */
  predicate: WatchPredicateSpec;
  mode: WatchMode;
  /** Every field the watch's agent is allowed to report, keyed by field
   *  name. */
  fields: Record<string, WatchFieldSpec>;
}

/** How an assignment fires. The union is internally tagged on `type`; the tag
 *  values are capitalized (`"Cron"` / `"Webhook"` / `"ConnectorEvent"`) to
 *  match the backend wire shape exactly. */
export type AssignmentTrigger =
  | { type: "Cron"; cron_expr: string; is_recurring: boolean }
  | {
      type: "Webhook";
      /** Deprecated legacy shared-secret check (`X-Assignment-Token` header
       *  or `?token=`). Superseded by `route_name` + `secret_ref`-based HMAC
       *  signing; carried through unchanged by the editor for rows that
       *  predate the named-route gateway, never edited via a UI control. */
      token: string | null;
      /** Named route this trigger is served under (`POST
       *  /webhooks/<route_name>`). `null` for legacy rows that predate the
       *  named-route gateway. */
      route_name?: string | null;
      /** Secret-store key this route's HMAC signing secret is resolved
       *  from. The editor always sets this equal to `route_name` — the
       *  backend also falls back to `route_name` when this is absent, so
       *  the two are a real 1:1 equivalence, not just a UI convention. */
      secret_ref?: string | null;
      /** Event-type allowlist (e.g. a GitHub `X-GitHub-Event` value like
       *  `"pull_request"`). Empty means every event type passes through to
       *  `filters`. */
      events: string[];
      /** Declarative, pre-agent relevance filter evaluated against the raw
       *  inbound JSON payload. `null`/absent means no filtering beyond
       *  `events`. */
      filters?: WebhookFilter | null;
      /** Prompt template rendered with `{dot.path}` payload field refs and a
       *  `{__raw__}` escape hatch, handed to the agent as its instruction.
       *  `null`/absent falls back to the assignment's static `instruction`. */
      prompt_template?: string | null;
      /** Where a payload that passes `events`/`filters` is routed. */
      deliver: WebhookDeliverTarget;
    }
  | {
      type: "ConnectorEvent";
      /** MCP server / connector id, matching an entry the connector was
       *  added under (e.g. an `mcp_servers.toml` name). */
      server_name: string;
      /** What to poll and how to extract the dedup cursor. */
      poll: ConnectorPollSpec;
      /** Minimum seconds between polls. */
      poll_interval_secs: number;
    }
  | {
      type: "AgentWatch";
      /** The plain-language watch condition, e.g. "check my inbox for a new
       *  email from finance and summarize it." Handed to the assignment's
       *  own agent on every due poll — there is no `tool_name`/`arguments`/
       *  `cursor_path` to configure, which is the entire point of this tier:
       *  any MCP connector works day one, with the agent itself deciding
       *  which tools to call. Mirrors `AssignmentTrigger::AgentWatch` on the
       *  backend. */
      instruction: string;
      /** Minimum seconds between detect-loop evaluations. Same floor-clamp
       *  convention as `ConnectorEvent::poll_interval_secs`. */
      poll_interval_secs: number;
      /** Restricts the watch agent to one MCP connector's tools
       *  (`mcp__{connector_scope}__*`). `null`/absent (the default) leaves
       *  it free to use every configured MCP tool. Mirrors
       *  `AssignmentTrigger::AgentWatch::connector_scope` on the backend —
       *  optional so watches persisted before this field existed still
       *  deserialize unchanged. */
      connector_scope?: string | null;
      /** The frozen declaration of what this watch observes and what
       *  identity, change, and firing mean for it —
       *  the backend's watch-contract types. `null`/absent until the watch's
       *  first run authors one; every poll after that binds the model to it
       *  instead of letting it re-decide identity from scratch. Mirrors
       *  `AssignmentTrigger::AgentWatch::contract` on the backend. */
      contract?: WatchContract | null;
      /** Bare name (no `mcp__<connector>__` prefix) of the read-only tool
       *  the authoring run self-reported using to answer this watch.
       *  `null`/absent until authoring runs, or when the authoring reply
       *  never self-reported one — in that case this watch stays fully
       *  model-driven, and `watch_health.extraction_plan_degraded_reason`
       *  explains why. Never inferred from anything else — displayed
       *  exactly as reported, since this is the tool a scheduled poll will
       *  call with no model reviewing it. Mirrors
       *  `AssignmentTrigger::AgentWatch::extraction_tool` on the backend. */
      extraction_tool?: string | null;
      /** The exact arguments `extraction_tool` was self-reported as having
       *  been called with. Only meaningful alongside `extraction_tool`.
       *  Mirrors `AssignmentTrigger::AgentWatch::extraction_args` on the
       *  backend. */
      extraction_args?: unknown;
    };

/** The complete, closed set of reasons an assignment's tick can end WITHOUT
 *  firing — the machine-readable half of the liveness contract. Mirrors
 *  `ao_protocol::assignment::QuiescenceReason` on the backend; a
 *  discriminated union on `kind` (internally-tagged there too, so this shape
 *  matches the wire JSON exactly, including `agent_watch_contract_not_bound`,
 *  whose fields are the wrapped [[WatchContractStatus]]'s own `state` variant
 *  merged alongside `kind`). Never re-derive prose from this client-side —
 *  render `AssignmentWatchHealth.quiescence_explanation` instead, which the
 *  backend already renders from this same value. */
export type QuiescenceReason =
  | { kind: "expired"; expires_at: string }
  | { kind: "not_due"; next_fire_at: string | null }
  | { kind: "fire_failed"; reason: string }
  | { kind: "server_not_connected"; server: string; state: string | null }
  | { kind: "no_live_handle"; server: string }
  | { kind: "poll_failed"; server: string; reason: string }
  | { kind: "cursor_unresolved"; server: string }
  | ({ kind: "agent_watch_contract_not_bound" } & WatchContractStatus);

/** How much an `AgentWatch`'s extraction can be trusted — three very
 *  different health states, not shades of the same thing (standing product
 *  rule: "if the engine detects it, the user sees it," no silent
 *  degradation). Mirrors `extractor_contract::Tier` on the backend.
 *
 *  - `deterministic` — exact items extracted with zero model involvement.
 *    Healthy.
 *  - `probabilistic` — extraction worked, but inference was involved; it may
 *    be wrong. Degraded, still usable.
 *  - `change_detection_only` — the watch can tell something changed but not
 *    what. The honest-failure state; must render as visibly unhealthy with
 *    its concrete cause attached, never a silent green. */
export type WatchExtractionTier = "deterministic" | "probabilistic" | "change_detection_only";

/** Which mechanism actually produced an `AgentWatch`'s last poll's
 *  candidates — purely informational. Mirrors
 *  `assignment_scratchpad::ExtractionPath` on the backend. */
export type WatchExtractionPath = "unbound" | "llm" | "deterministic" | "probabilistic";

/** Whether an `AgentWatch`'s steady-state poll can skip the model entirely —
 *  distinct from `WatchExtractionTier`/`WatchExtractionPath`, which alone
 *  cannot tell "this watch runs the model every poll because no extraction
 *  plan could ever be authored for it" apart from "this watch has never
 *  polled" apart from "this watch is still mid-authoring." Mirrors
 *  `ao_protocol::assignment_scratchpad::ExtractionHealth` on the backend,
 *  derived server-side by `ao_engine::agent_watch::derive_extraction_health`
 *  — never re-derived here.
 *
 *  - `pending` — no poll has completed yet.
 *  - `deterministic` — a frozen extraction plan exists; direct-invoke
 *    resolves every poll with zero model calls. Healthy.
 *  - `model_assisted` — the tool + args may be frozen, but no extraction
 *    plan was ever authored (the source returns no structured content); a
 *    model reads its output on every poll.
 *  - `degraded` — a plan existed and direct-invoke failed; the watch fell
 *    back to model extraction. */
export type WatchExtractionHealth = "pending" | "deterministic" | "model_assisted" | "degraded";

/** Whether the last poll's zero-model-call extraction is backed by a
 *  server-declared schema, or was reconstructed by parsing text out of a
 *  response the server never declared a schema for. Mirrors
 *  `ao_server::routes::assignments::extraction_provenance_wire_str` on the
 *  backend.
 *
 *  - `declared_schema` — the strongest guarantee: the server contractually
 *    promises this shape.
 *  - `parsed_from_text` — a parsed guess with no server promise behind it;
 *    it can drift without notice. */
export type WatchExtractionProvenance = "declared_schema" | "parsed_from_text";

/** Which of three MUTUALLY EXCLUSIVE contract-authoring states an `AgentWatch`
 *  assignment is in — the single source of truth `WatchContractPanel`
 *  branches its top-level rendering on. Mirrors
 *  `ao_engine::agent_watch::WatchContractStatus` on the backend,
 *  server-derived; never re-derive this client-side, and never infer "is a
 *  contract bound" from `tier`/`extraction_health`/`AssignmentWatchHealth`'s
 *  other fields, or from `Assignment.trigger`'s own `contract` presence —
 *  those two signals used to be checked independently by the UI and could
 *  both read true at once (a poll that recorded a scratchpad but whose
 *  authoring attempt was rejected), rendering two contradictory statements
 *  ("no contract yet" and "model-assisted, no fixed tier") on the same
 *  panel. `tier`/`extraction_health` still describe extraction-tier health
 *  WITHIN the `"bound"` state — this type only ever answers "is there a live
 *  contract to show."
 *
 *  A TS discriminated union on `state` — a `switch` can only ever take one
 *  arm, matching the backend `enum`'s own mutual exclusivity. */
export type WatchContractStatus =
  | {
      /** No poll has completed for this watch's CURRENT instruction/
       *  connector_scope yet — either it has never polled, or its
       *  contract-derived scratchpad state was just invalidated by an edit
       *  (see `AssignmentScratchpad::invalidate_watch_contract_state` on the
       *  backend) and the next poll hasn't run yet. */
      state: "not_yet_attempted";
    }
  | {
      /** At least one poll has run authoring for the current input and
       *  every attempt so far was rejected, or offered no proposal at all —
       *  no `WatchContract` is bound yet. */
      state: "authoring_rejected";
      /** Consecutive polls that ended without a bound contract. */
      attempts: number;
      /** `true` once `attempts` has reached the authoring ceiling —
       *  authoring has stopped re-prompting until the instruction or
       *  connector scope is edited. */
      ceiling_hit: boolean;
      /** The most recent concrete validation failure, when one was ever
       *  recorded. `null` when a poll offered no proposal at all yet (this
       *  never blanks out a real prior reason). */
      last_rejection_reason: string | null;
    }
  | {
      /** A `WatchContract` is bound. Extraction-tier detail
       *  (deterministic/probabilistic/model-assisted extraction/degraded)
       *  is unrelated to this state and still lives on this
       *  `AssignmentWatchHealth`'s own `tier`/`extraction_health` fields. */
      state: "bound";
      /** `Some(n)` (here, a non-null `n`) when the live contract only bound
       *  after `n` rejected proposals — render this explicitly instead of
       *  leaving an earlier rejection looking unresolved. `null` when it
       *  bound cleanly on the first attempt. */
      bound_after_repairs: number | null;
    };

/** Liveness + health payload joined onto the assignment response for EVERY
 *  trigger kind — never itself something the editor writes back. Mirrors
 *  `ao_server::routes::assignments::AssignmentWatchHealth` on the backend.
 *
 *  Originally `AgentWatch`-only (hence the name, kept for the existing
 *  frontend consumer): the extraction-tier fields below (`tier` through
 *  `contract_status`) only ever carry real data for an `AgentWatch` trigger
 *  and read at their neutral "nothing to report" defaults for `Cron`/
 *  `ConnectorEvent` (see each field's own doc). `last_evaluated_at` through
 *  `quiescence_explanation` at the bottom of this interface are populated
 *  the same way for every trigger kind, straight off the backend's
 *  `Assignment.liveness` — render [[AssignmentLivenessBadge]] rather than
 *  [[AssignmentWatchHealthBadge]] directly for a non-`AgentWatch` row. */
export interface AssignmentWatchHealth {
  /** `false` until this watch's first poll has produced a scratchpad — a
   *  watch that has never evaluated must render distinctly from one that
   *  evaluated and bound cleanly ("unknown" and "healthy" must not look the
   *  same). */
  has_evaluated: boolean;
  /** `null` on an evaluated watch with no tier to report yet (contract still
   *  being authored, or a bound contract with no extraction plan — every
   *  poll runs the full model detector instead). */
  tier: WatchExtractionTier | null;
  extraction_path: WatchExtractionPath | null;
  /** The concrete, engine-derived cause the extraction plan could only hash
   *  the whole response instead of extracting individual items. Set exactly
   *  when `tier` is `"change_detection_only"`. */
  degraded_reason: string | null;
  /** `true` once a poll's extraction plan failed structurally and this watch
   *  fell back to the model for that poll — distinct from `degraded_reason`
   *  above, which only ever explains a `"change_detection_only"` tier, a
   *  normal, expected state. A watch on this fallback is never healthy,
   *  regardless of what `tier` reads (a poll that took this branch reports
   *  `tier: null`, the same "nothing to claim" value an unbound watch
   *  reports — this flag is what tells the two apart). */
  extraction_plan_degraded: boolean;
  /** The structured cause of the extraction plan's failure (the available-
   *  paths list or the excerpt that failed to match), as persisted by the
   *  engine. `null` unless `extraction_plan_degraded` is `true`. Render this
   *  string as-is — it is the actual, engine-derived reason, not placeholder
   *  copy. */
  extraction_plan_degraded_reason: string | null;
  /** The bound contract's fire condition, rendered by the backend back into
   *  the readable expression grammar for the explainer's "show raw
   *  expression" disclosure.
   *
   *  Response-only and derived per request — deliberately not a field on the
   *  contract itself, since the contract is fingerprinted and a new field
   *  inside it would force a snapshot reseed on every existing watch.
   *
   *  `null`/absent when no contract is bound yet, or when the predicate uses
   *  comparisons the display grammar cannot express. Render an explicit
   *  message for that case — never an empty box. */
  predicate_expr?: string | null;
  /** Per-day count of LLM child sessions this watch's detector has spawned,
   *  keyed by UTC calendar date (`YYYY-MM-DD`) — the only usage/cost
   *  telemetry this system tracks. Empty/absent on a watch that has never
   *  spawned a model session. */
  model_calls_by_day?: Record<string, number>;
  /** Count of consecutive completed polls that produced zero newly-fired
   *  items. Informational only — a high value does not mean this watch is
   *  unhealthy, it may simply be quiet. */
  consecutive_polls_without_new_items?: number;
  /** RFC3339 timestamp of the last poll that fired at least one item. `null`
   *  until this watch's first-ever fire. */
  last_new_item_at?: string | null;
  /** Whether this watch's steady-state poll can skip the model entirely —
   *  see [[WatchExtractionHealth]]. Server-derived; never re-derive this
   *  client-side. Optional/absent only for a response captured before this
   *  field existed. */
  extraction_health?: WatchExtractionHealth;
  /** Human-readable reason for `extraction_health`, shown verbatim. `null`
   *  for `"pending"`/`"deterministic"` (nothing to explain); set for
   *  `"model_assisted"` and `"degraded"`. */
  extraction_health_reason?: string | null;
  /** Today's entry in `model_calls_by_day` (UTC calendar date), `0` if this
   *  watch has spawned no model session yet today. */
  model_calls_today?: number;
  /** Same value as `extraction_path`, as a plain string rather than the
   *  typed `WatchExtractionPath` union — `null` exactly when
   *  `extraction_path` is `null`. */
  last_extraction_path?: string | null;
  /** `true` once a bound `native_id` contract's stability probe came back
   *  inconclusive rather than confirmed stable, straight off the backend's
   *  persisted scratchpad. The watch runs normally either way — an
   *  inconclusive probe never drops a rung, only a positive instability
   *  finding does — but this is what makes "this watch's identity was
   *  never actually verified across polls" visible instead of
   *  indistinguishable from one that was. Server-derived; never re-derive
   *  this client-side. */
  identity_probe_inconclusive?: boolean;
  /** Human-readable reason for `identity_probe_inconclusive`, shown
   *  verbatim. `null` unless that flag is `true`. */
  identity_probe_inconclusive_reason?: string | null;
  /** Whether the last poll's zero-model-call extraction is backed by a
   *  server-declared schema or was reconstructed by parsing text — see
   *  [[WatchExtractionProvenance]]. This exists because `extraction_health:
   *  "deterministic"` alone conflates the two: the cost claim its frozen-
   *  contract disclosure makes ("no model reviews this before it runs") is
   *  true either way, but says nothing about DRIFT RISK, which provenance
   *  predicts and cost does not. Server-derived; never re-derive this
   *  client-side. `null` whenever the last poll didn't actually resolve a
   *  plan (still model-assisted, unbound, or a response captured before
   *  this field existed) — that absence must never be read as a
   *  "declared schema" guarantee it didn't earn. */
  extraction_provenance?: WatchExtractionProvenance | null;
  /** The single source of truth for which of three mutually exclusive
   *  contract-authoring states this watch is in — see
   *  [[WatchContractStatus]]. Always present on a real response; a client
   *  MUST branch on this field alone for "is a contract bound yet," never
   *  re-derive it. */
  contract_status: WatchContractStatus;

  // -- Generic liveness fields below: populated identically for every
  // -- trigger kind, straight off the backend's `Assignment.liveness`.
  // -- Optional (like most fields above) so existing fixtures/tests built
  // -- before these fields existed keep type-checking; a real API response
  // -- always includes them (the backend never omits them, only nulls them).
  /** When the tick loop most recently evaluated this assignment at all,
   *  regardless of trigger kind or whether that tick fired. `null`/absent
   *  until the very first tick ever looks at this assignment. This, together
   *  with `quiescence_reason` below, is what tells a never-evaluated
   *  assignment apart from one that was evaluated and correctly chose not to
   *  fire — collapsing those two readings is the exact bug class this field
   *  exists to eliminate: never infer "never evaluated" from the absence of
   *  a `quiescence_reason` alone. */
  last_evaluated_at?: string | null;
  /** Total number of times this assignment has fired, over its whole
   *  lifetime. `0`/absent for an assignment that has never fired, whether or
   *  not it has ever been evaluated. */
  fire_count?: number;
  /** Why the most recent tick ended without firing, machine-readable — see
   *  [[QuiescenceReason]]. `null`/absent when either this assignment has
   *  never been evaluated, or its most recent tick fired. Present for every
   *  trigger kind, not just `AgentWatch`. Never switch on this client-side
   *  to produce prose — render `quiescence_explanation` below instead. */
  quiescence_reason?: QuiescenceReason | null;
  /** One plain-English sentence, rendered server-side, naming the specific
   *  cause behind `quiescence_reason` (e.g. which MCP server is
   *  disconnected) rather than a generic "unavailable" string. `null`/absent
   *  exactly when `quiescence_reason` is absent. This is the actual product
   *  surface for "why have I not fired?" — render it directly. */
  quiescence_explanation?: string | null;
}

/** How an assignment run's output is handled after completion. v1 captures the
 *  output in the run row only. */
export type OutputMode = "background";

/** Where an assignment's runs land. `fresh` (the default) creates a new
 *  throwaway thread every fire — a run never interrupts whatever thread the
 *  user is actively chatting in, but each run starts cold. `main` posts into
 *  the agent's main/default thread, the same one interactive chat uses —
 *  good for a run that should read like part of an ongoing conversation
 *  (e.g. a coach). `dedicated` reuses one thread across every fire, created
 *  automatically on the first fire — good for a run that should accumulate
 *  its own running history (e.g. a daily brief). */
export type AssignmentThreadPolicy = "fresh" | "main" | "dedicated";

/** Forward-compatibility placeholder for MCP-server / connector bindings. v1
 *  carries no enforced semantics; callers may omit it entirely. */
export interface AssignmentBinding {
  kind: string;
  ref_id: string;
}

/** A persistent rule that drives proactive agent runs on a trigger. One
 *  assignment may produce many runs over time. */
export interface Assignment {
  id: AssignmentId;
  agent_id: string;
  name: string;
  instruction: string;
  /** Focus path passed through as the fired run's `focus_path`. `null`/absent
   *  runs with no working-directory override. */
  working_directory?: string | null;
  trigger: AssignmentTrigger;
  bindings: AssignmentBinding[];
  output_mode: OutputMode;
  thread_policy: AssignmentThreadPolicy;
  /** Set once a `dedicated`-policy assignment has fired at least once; the
   *  same thread id is reused on every subsequent fire. Absent/null for
   *  `fresh`/`main` assignments and for a `dedicated` assignment that has
   *  never fired. Server-managed — never client-settable. */
  dedicated_thread_id?: string | null;
  enabled: boolean;
  /** Optional cutoff after which the assignment stops firing; `null`/absent
   *  means it never expires on its own. */
  expires_at?: string | null;
  /** Scheduling state for Cron triggers; always null for Webhook triggers. */
  next_fire_at?: string | null;
  last_run_at?: string | null;
  created_ts: string;
  updated_ts: string;
  /** Liveness + health payload, present for EVERY trigger kind (see
   *  [[AssignmentWatchHealth]]'s own doc) — not just `AgentWatch`, despite
   *  the field's name. Render via [[AssignmentLivenessBadge]], which
   *  branches on trigger kind for you. */
  watch_health?: AssignmentWatchHealth | null;
}

/** What class of trigger fired a run. */
export type AssignmentTriggerKind = "cron" | "webhook" | "manual" | "connector_event" | "agent_watch";

/** Lifecycle state of a single proactive execution. */
export type AssignmentRunStatus = "queued" | "running" | "succeeded" | "failed";

/** One proactive execution of an assignment. */
export interface AssignmentRun {
  id: string;
  assignment_id: AssignmentId;
  agent_id: string;
  trigger_kind: AssignmentTriggerKind;
  trigger_payload?: string | null;
  status: AssignmentRunStatus;
  output_summary?: string | null;
  /** The thread holding this run's conversation, resolved per the
   *  assignment's thread_policy at fire time (a fresh thread, the agent's
   *  main thread, or the assignment's reused dedicated thread). */
  thread_id?: string | null;
  queued_at: string;
  started_ts?: string | null;
  finished_ts?: string | null;
  error?: string | null;
}
