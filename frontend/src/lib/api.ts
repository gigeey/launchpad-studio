import type { AgentSnapshot, AgentProfile, PluginEnablement, MessageAck, MemoryEntry, MemoryScope, ReflectionCandidate, BookmarkEntry, PaginationCursor, PaginatedMessagesResponse, ProjectMessagesResponse, Attachment, Artifact, ArtifactWithPayload, CreateArtifactRequest, PinnedArtifact, ArtifactGroup, TranscriptEntry, Tasklist, ListTasklistsResponse, CreateTasklistRequest, AppendTaskRequest, AddTaskCommentRequest, TaskComment, Project, ProjectListItem, TasklistScope, Thread, ThreadKind, BranchSource, Assignment, AssignmentId, AssignmentTrigger, AssignmentBinding, OutputMode, AssignmentThreadPolicy, AssignmentTriggerKind, AssignmentRunStatus, AssignmentRun, WebhookFieldFilter, WebhookFilter, WebhookDeliverTarget } from "../types/api";
import type { WorkflowSummary, WorkflowDefinition, RefreshResponse, CreateTaskRequest, CreateTaskResponse, TaskSummary, TaskSnapshot } from "../types/workflow";
import type { FormAction, FormAnswerMap } from "../types/form";
export type { AgentProfile, PluginEnablement, MemoryEntry, BookmarkEntry, PaginationCursor, PaginatedMessagesResponse, ProjectMessagesResponse, Attachment, Artifact, ArtifactWithPayload, CreateArtifactRequest, PinnedArtifact, ArtifactGroup, Project, ProjectListItem };
export type { WorkflowSummary, WorkflowDefinition, TaskSummary, TaskSnapshot };
export type { Tasklist, ListTasklistsResponse, CreateTasklistRequest, AppendTaskRequest, AddTaskCommentRequest, TaskComment };
export type { TasklistScope };
export type { Thread, ThreadKind, BranchSource };
export type { Assignment, AssignmentId, AssignmentTrigger, AssignmentBinding, OutputMode, AssignmentThreadPolicy, AssignmentTriggerKind, AssignmentRunStatus, AssignmentRun, WebhookFieldFilter, WebhookFilter, WebhookDeliverTarget };

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

export const BASE_URL = import.meta.env.VITE_API_BASE_URL ?? "http://localhost:3001";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Default per-request timeout applied by {@link fetchJson} when a call site
 *  doesn't override it. Exported so tests (and any future caller that wants
 *  a non-default deadline) can reference the same value instead of
 *  duplicating the magic number. Chosen to comfortably exceed ordinary
 *  round-trips while still failing a genuinely hung request well before a
 *  human would give up watching a skeleton. */
export const DEFAULT_FETCH_TIMEOUT_MS = 15_000;

/** Deadline for handlers whose duration scales with the size of the user's
 *  own data rather than with network conditions — recursive directory
 *  copies, walks across every agent on disk, ripgrep sweeps over a whole
 *  transcript history. These are local disk or subprocess work, so they
 *  don't hang indefinitely the way a dead socket does; they just take
 *  proportionally longer for a user with a large workspace. The
 *  {@link DEFAULT_FETCH_TIMEOUT_MS} default is deliberately tight enough to
 *  abort these mid-flight and surface a spurious error, so each such call
 *  site passes this explicitly as `fetchJson`'s third argument. */
export const BULK_FETCH_TIMEOUT_MS = 60_000;

/** Deadline for handlers that block on a third party we do not control — an
 *  LLM provider round-trip, a paginating remote API, or a spawned
 *  subprocess handshake. Unlike {@link BULK_FETCH_TIMEOUT_MS} the duration
 *  here isn't bounded by anything local, so the ceiling is set by how long
 *  a user will plausibly wait on a deliberate action (they pressed a button
 *  and expect model output) rather than by an estimate of the work. Still
 *  finite: an upstream that has genuinely stopped responding must not leave
 *  a `loading` flag stuck forever. */
export const EXTERNAL_WORK_TIMEOUT_MS = 120_000;

/** Thrown by {@link fetchJson} when a request is aborted because it exceeded
 *  its timeout — as opposed to being cancelled via the caller's own
 *  `init.signal` (which rejects with the signal's own abort reason,
 *  ordinarily a plain `AbortError`) or failing with a non-2xx HTTP status
 *  (a plain `Error`). `name === "TimeoutError"` mirrors the native
 *  `AbortSignal.timeout()` convention already used elsewhere in this file
 *  (see {@link getProviderModels}), so callers can check `err.name` without
 *  an `instanceof` import. */
export class FetchTimeoutError extends Error {
  constructor(path: string, timeoutMs: number) {
    super(`Request to ${path} timed out after ${timeoutMs}ms`);
    this.name = "TimeoutError";
  }
}

/** Shared fetch+JSON wrapper used by nearly every function below. Bounds
 *  every request with a timeout so a hung network call can't leave a
 *  caller's `loading` flag stuck forever. If the caller passes its own
 *  `init.signal`, the request aborts on EITHER that signal OR
 *  the timeout — whichever fires first — via `AbortSignal.any` where
 *  available, falling back to a manual forwarding listener otherwise.
 *  Exported so `__tests__/api.fetchJson.test.ts` can exercise it directly;
 *  every existing call site elsewhere in this file only ever passes
 *  `path`/`init` and stays source-compatible — the optional `timeoutMs`
 *  param exists purely so tests (and any future caller) can override the
 *  default without touching any public function's signature. */
export async function fetchJson<T>(path: string, init: RequestInit = {}, timeoutMs: number = DEFAULT_FETCH_TIMEOUT_MS): Promise<T> {
  const callerSignal = init.signal ?? undefined;
  const timeoutController = new AbortController();
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    timeoutController.abort();
  }, timeoutMs);

  let signal: AbortSignal = timeoutController.signal;
  let unforward: (() => void) | undefined;
  if (callerSignal) {
    if (typeof AbortSignal.any === "function") {
      // Compose without clobbering: the actual fetch aborts on whichever of
      // the two signals fires first.
      signal = AbortSignal.any([callerSignal, timeoutController.signal]);
    } else {
      // No AbortSignal.any in this runtime — forward the caller's abort into
      // our own controller so it still cancels the underlying fetch.
      if (callerSignal.aborted) {
        timeoutController.abort();
      } else {
        const forward = () => timeoutController.abort();
        callerSignal.addEventListener("abort", forward, { once: true });
        unforward = () => callerSignal.removeEventListener("abort", forward);
      }
    }
  }

  try {
    const res = await fetch(`${BASE_URL}${path}`, { ...init, signal });
    if (!res.ok) {
      const body = await res.text().catch(() => "");
      throw new Error(`API ${res.status}: ${body}`);
    }
    return (await res.json()) as T;
  } catch (err) {
    // Only our own timer sets `timedOut` — a caller-initiated abort (via
    // AbortSignal.any or the manual fallback above) leaves it false, so the
    // original abort error (distinguishable as-is, ordinarily
    // `name === "AbortError"`) propagates unchanged.
    if (timedOut) {
      throw new FetchTimeoutError(path, timeoutMs);
    }
    throw err;
  } finally {
    clearTimeout(timer);
    unforward?.();
  }
}

// ---------------------------------------------------------------------------
// API functions
// ---------------------------------------------------------------------------

/** Fetch all agent snapshots. */
export async function getAgents(): Promise<AgentSnapshot[]> {
  return fetchJson<AgentSnapshot[]>("/agents");
}

/** Fetch full agent profile by id. Pass `signal` so a superseded caller
 *  (e.g. chatStore's `selectAgent`, on a fast agent/thread switch) can
 *  actually cancel the request instead of merely ignoring its result on
 *  arrival — composed with `fetchJson`'s own timeout signal, not clobbering
 *  it (see `fetchJson`'s doc comment). */
export async function getAgent(id: string, signal?: AbortSignal): Promise<AgentProfile> {
  return fetchJson<AgentProfile>(`/agents/${encodeURIComponent(id)}`, { signal });
}

/** Fetch the transcript (messages) for an agent. Pass `threadId` for a
 *  non-default thread. `signal` is forwarded straight through to
 *  `fetchJson` — see `getAgent`'s doc comment on why callers pass it. */
export async function getMessages(agentId: string, threadId?: string, signal?: AbortSignal): Promise<PaginatedMessagesResponse> {
  const params = threadId ? `?thread_id=${encodeURIComponent(threadId)}` : "";
  return fetchJson<PaginatedMessagesResponse>(`/agents/${encodeURIComponent(agentId)}/messages${params}`, { signal });
}

/** Fetch messages before a cursor position for an agent. Pass `threadId` for a non-default thread.
 *  `cursor.phase` is round-tripped verbatim as `cursor_phase` — once a branch
 *  thread's own history is exhausted, the server hands back an `"inherited"`
 *  cursor addressing the SOURCE thread's transcript instead, and this must be
 *  echoed back so "load older" keeps reading from the right file. `signal` is
 *  forwarded to `fetchJson` — see `getAgent`'s doc comment. */
export async function getMessagesBefore(agentId: string, cursor: PaginationCursor, last = 50, threadId?: string, signal?: AbortSignal): Promise<PaginatedMessagesResponse> {
  const params = new URLSearchParams({
    cursor_offset: String(cursor.byte_offset),
    cursor_message_id: cursor.last_message_id,
    cursor_timestamp: cursor.timestamp,
    last: String(last),
  });
  if (cursor.phase) params.set("cursor_phase", cursor.phase);
  if (threadId) params.set("thread_id", threadId);
  return fetchJson<PaginatedMessagesResponse>(`/agents/${encodeURIComponent(agentId)}/messages?${params}`, { signal });
}

/** Send a new message to an agent. Pass `threadId` to route to a non-default thread. */
export async function sendMessage(agentId: string, content: string, attachmentIds?: string[], focusPath?: string | null, threadId?: string): Promise<MessageAck> {
  const payload: Record<string, unknown> = { content };
  if (attachmentIds && attachmentIds.length > 0) {
    payload.attachment_ids = attachmentIds;
  }
  if (focusPath) {
    payload.focus_path = focusPath;
  }
  if (threadId) {
    payload.thread_id = threadId;
  }
  return fetchJson<MessageAck>(`/agents/${encodeURIComponent(agentId)}/messages`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
}

/** Cancel the active run for an agent on a given thread. Pass `threadId` for
 *  a non-default thread — omitting it targets the default/no-thread
 *  conversation only, NOT every thread for this agent (an agent can have
 *  concurrent runs across multiple threads; this scopes the cancel to one). */
export async function cancelAgentRun(agentId: string, threadId?: string): Promise<void> {
  const params = threadId ? `?thread_id=${encodeURIComponent(threadId)}` : "";
  await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/cancel${params}`, { method: "POST" });
}

/** Cancel one in-flight async `Delegate` run (POST /delegates/{delegation_id}/cancel).
 *  Idempotent — cancelling an already-cancelled delegation returns 200 with
 *  `status: "already_cancelled"` rather than erroring, so callers never need
 *  to guard against double-clicks. Scoped to exactly one delegation; there
 *  is deliberately no cancel-all route on the server (see
 *  `crates/ao-server/src/routes/delegates.rs`) — a caller that wants to
 *  cancel every delegation on a thread calls this once per delegation id
 *  (see `DelegatePillRow`'s kill-all button). */
export async function cancelDelegate(delegationId: string): Promise<{ status: string; id: string }> {
  return fetchJson(`/delegates/${encodeURIComponent(delegationId)}/cancel`, { method: "POST" });
}

/** Submit answers for an async form (POST /agents/{id}/async-forms/{form_id}/answer). */
export async function submitAsyncFormAnswer(
  agentId: string,
  formId: string,
  values: Record<string, unknown>,
): Promise<{ message_id: string; status: string }> {
  return fetchJson<{ message_id: string; status: string }>(
    `/agents/${encodeURIComponent(agentId)}/async-forms/${encodeURIComponent(formId)}/answer`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ values }),
    },
  );
}

/** Dismiss an async form (POST /agents/{id}/async-forms/{form_id}/dismiss). */
export async function dismissAsyncForm(
  agentId: string,
  formId: string,
): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/async-forms/${encodeURIComponent(formId)}/dismiss`,
    { method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" },
  );
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${text}`);
  }
}

/** Submit answers for a project async form (POST /projects/{id}/async-forms/{form_id}/answer). */
export async function submitAsyncFormAnswerForProject(
  projectId: string,
  formId: string,
  values: Record<string, unknown>,
): Promise<{ message_id: string; status: string }> {
  return fetchJson<{ message_id: string; status: string }>(
    `/projects/${encodeURIComponent(projectId)}/async-forms/${encodeURIComponent(formId)}/answer`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ values }),
    },
  );
}

/** Dismiss a project async form (POST /projects/{id}/async-forms/{form_id}/dismiss). */
export async function dismissAsyncFormForProject(
  projectId: string,
  formId: string,
): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/projects/${encodeURIComponent(projectId)}/async-forms/${encodeURIComponent(formId)}/dismiss`,
    { method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" },
  );
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${text}`);
  }
}

/** Submit answers for an active form prompt (AskUserQuestionWithForm).
 *
 *  Also doubles as the action-button delivery path: pass `action` (with
 *  empty `answers`) instead of a filled `answers` map to deliver a Cancel /
 *  Regenerate / Something-else click — it resolves the same suspended tool
 *  call through the same route, just with a different `FormResponse` shape
 *  server-side. */
export async function submitFormAnswer(
  agentId: string,
  body: { form_id: string; answers: FormAnswerMap; action?: FormAction; note?: string },
): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/form-answer`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    },
  );
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${text}`);
  }
}

/** Fire-and-forget: eagerly precompute context for an agent so first message is fast. */
export function precomputeContext(agentId: string, focusPath?: string | null): void {
  const payload: Record<string, unknown> = {};
  if (focusPath) {
    payload.focus_path = focusPath;
  }
  fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/precompute-context`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  }).catch(() => {});
}

/** Create a new agent. */
export async function createAgent(profile: AgentProfile): Promise<AgentProfile> {
  return fetchJson<AgentProfile>("/agents", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(profile),
  });
}

/** Update an existing agent. */
export async function updateAgent(profile: AgentProfile): Promise<AgentProfile> {
  return fetchJson<AgentProfile>(`/agents/${encodeURIComponent(profile.id)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(profile),
  });
}

/** Fetch the canonical composed system prompt for an agent (empty volatile tail). */
export async function getComposedPrompt(agentId: string): Promise<string> {
  return fetchJson<string>(`/agents/${encodeURIComponent(agentId)}/compose-prompt`);
}

/** Clone an existing agent; returns the new agent profile (name = "<Parent> - copy"). */
export async function cloneAgent(parentId: string): Promise<AgentProfile> {
  // Recursive copy of the parent's whole agent home dir (skills, rules,
  // memory) — duration scales with that agent's on-disk size.
  return fetchJson<AgentProfile>(`/agents/${encodeURIComponent(parentId)}/clone`, {
    method: "POST",
  }, BULK_FETCH_TIMEOUT_MS);
}

/** Error thrown by API calls when the HTTP status is non-2xx. `status` lets
 *  callers distinguish structured failures (e.g. 409 coordinator rejection). */
export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

/** Delete an agent by id. Cascades team-membership cleanup and home-directory
 *  removal on the server. Throws `ApiError` with `status === 409` if the agent
 *  is a coordinator of any team. */
export async function deleteAgent(id: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(id)}`, { method: "DELETE" });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let message = body || `Request failed with status ${res.status}`;
    try {
      const parsed = JSON.parse(body);
      if (parsed && typeof parsed.error === "string") message = parsed.error;
    } catch {
      // body wasn't JSON — fall through with raw text
    }
    throw new ApiError(res.status, message);
  }
}

export interface TelegramTokenResult {
  bot_username: string;
}

export interface TelegramStatus {
  has_token: boolean;
  bot_username: string | null;
  enabled: boolean;
  linked: boolean;
  // Optional here (rather than required) so pre-existing local TelegramStatus
  // literals elsewhere don't need updating in this change; the backend always
  // sends both fields on GET …/telegram/status.
  allowed_chat_ids?: number[];
  pending_pairing_code?: PairingCode | null;
}

export interface PairingCode {
  code: string;
  expires_at_unix: number;
}

/** Validate and store a Telegram bot token for an agent (`PUT …/telegram/token`).
 *  Write-only: the token itself is never returned. Runs a live `getMe` call
 *  server-side before storing, so throws `ApiError` (400) with the backend's
 *  message on an invalid or empty token, or 404 for an unknown agent. */
export async function setTelegramToken(agentId: string, token: string): Promise<TelegramTokenResult> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/telegram/token`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token }),
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let message = body || `Request failed with status ${res.status}`;
    try {
      const parsed = JSON.parse(body);
      if (parsed && typeof parsed.error === "string") message = parsed.error;
    } catch {
      // body wasn't JSON — fall through with raw text
    }
    throw new ApiError(res.status, message);
  }
  return res.json() as Promise<TelegramTokenResult>;
}

/** Clear the stored Telegram token for an agent and disable the bridge
 *  (`DELETE …/telegram/token`). Resolves with no body on success (204). */
export async function deleteTelegramToken(agentId: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/telegram/token`, { method: "DELETE" });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let message = body || `Request failed with status ${res.status}`;
    try {
      const parsed = JSON.parse(body);
      if (parsed && typeof parsed.error === "string") message = parsed.error;
    } catch {
      // body wasn't JSON — fall through with raw text
    }
    throw new ApiError(res.status, message);
  }
}

/** Fetch non-secret Telegram bridge status for an agent (`GET …/telegram/status`) —
 *  never includes the token itself. */
export async function getTelegramStatus(agentId: string): Promise<TelegramStatus> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/telegram/status`);
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let message = body || `Request failed with status ${res.status}`;
    try {
      const parsed = JSON.parse(body);
      if (parsed && typeof parsed.error === "string") message = parsed.error;
    } catch {
      // body wasn't JSON — fall through with raw text
    }
    throw new ApiError(res.status, message);
  }
  return res.json() as Promise<TelegramStatus>;
}

/** Mint a fresh single-use Telegram pairing code for an agent
 *  (`POST …/telegram/pairing-code`), valid for 10 minutes. Regenerating
 *  overwrites any prior unclaimed code. */
export async function createTelegramPairingCode(agentId: string): Promise<PairingCode> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/telegram/pairing-code`, {
    method: "POST",
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let message = body || `Request failed with status ${res.status}`;
    try {
      const parsed = JSON.parse(body);
      if (parsed && typeof parsed.error === "string") message = parsed.error;
    } catch {
      // body wasn't JSON — fall through with raw text
    }
    throw new ApiError(res.status, message);
  }
  return res.json() as Promise<PairingCode>;
}

/** Unlink a paired Telegram chat from an agent
 *  (`DELETE …/telegram/chats/{chatId}`), returning the updated allow-list. */
export async function unlinkTelegramChat(agentId: string, chatId: number): Promise<{ allowed_chat_ids: number[] }> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/telegram/chats/${chatId}`, {
    method: "DELETE",
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let message = body || `Request failed with status ${res.status}`;
    try {
      const parsed = JSON.parse(body);
      if (parsed && typeof parsed.error === "string") message = parsed.error;
    } catch {
      // body wasn't JSON — fall through with raw text
    }
    throw new ApiError(res.status, message);
  }
  return res.json() as Promise<{ allowed_chat_ids: number[] }>;
}

// ---------------------------------------------------------------------------
// Channel bindings (generic status + Email config/secret/delete)
// ---------------------------------------------------------------------------

/** Honest per-binding connection state, as reported by the backend process
 *  answering the request (`ao_protocol::channel_connection_state::ChannelConnectionState`).
 *  `not-holding-lease` isn't an error: it means another backend process
 *  currently owns this connection (e.g. a second worktree's server pointed
 *  at the same data directory) — only one process may run a binding at a
 *  time, and this is the one that yielded. */
export type ChannelConnectionState = "connected" | "reconnecting" | "disconnected" | "not-holding-lease";

export interface ChannelStatus {
  binding_id: string;
  kind: "telegram" | "discord" | "email" | "slack" | "whatsapp" | "webhook";
  enabled: boolean;
  bridge_thread_provisioned: boolean;
  allowed_senders: string[];
  secret_stored: boolean;
  kind_config: Record<string, unknown>;
  connection_state: ChannelConnectionState;
}

/** Non-secret Email binding config (`ChannelKindConfig::Email`, minus the
 *  password, which only ever travels through `setEmailChannelSecret`). */
export interface EmailChannelConfig {
  address: string;
  imap_host: string;
  imap_port: number;
  smtp_host: string;
  smtp_port: number;
  poll_secs: number;
  require_auth_results: boolean;
  allowed_senders: string[];
  enabled: boolean;
}

async function throwApiError(res: Response): Promise<never> {
  const body = await res.text().catch(() => "");
  let message = body || `Request failed with status ${res.status}`;
  try {
    const parsed = JSON.parse(body);
    if (parsed && typeof parsed.error === "string") message = parsed.error;
  } catch {
    // body wasn't JSON — fall through with raw text
  }
  throw new ApiError(res.status, message);
}

/** Fetch every channel binding's non-secret status for an agent
 *  (`GET …/channels`) — includes Telegram (if configured) alongside Email
 *  for a unified view. Never includes a secret value, only whether one is
 *  on file (`secret_stored`). */
export async function getAgentChannels(agentId: string): Promise<ChannelStatus[]> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels`);
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<ChannelStatus[]>;
}

export interface ChannelSenders {
  senders: string[];
}

/** Fetch one channel binding's sender allow-list directly from the
 *  clobber-free store (`GET …/channels/{bindingId}/senders`) —
 *  the authoritative source for a binding's `allowed_senders`. Unlike
 *  `ChannelStatus.allowed_senders` (which mirrors the deprecated inline
 *  profile copy and can go stale once a binding's allow-list is edited
 *  through this dedicated route), this always reflects what was actually
 *  last saved. Throws `ApiError` (404) for an unknown agent. */
export async function getChannelSenders(agentId: string, bindingId: string): Promise<ChannelSenders> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/${encodeURIComponent(bindingId)}/senders`,
  );
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<ChannelSenders>;
}

/** Replace one channel binding's sender allow-list
 *  (`PUT …/channels/{bindingId}/senders`) without touching any other part of
 *  the binding's config. This is the clobber-free counterpart to the
 *  general `PUT /agents/{id}` profile save, which now ignores any
 *  client-supplied `allowed_senders` rather than let it race an
 *  out-of-band linker (e.g. Telegram pairing). Throws `ApiError` (404) for
 *  an unknown agent. */
export async function setChannelSenders(agentId: string, bindingId: string, senders: string[]): Promise<ChannelSenders> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/${encodeURIComponent(bindingId)}/senders`,
    {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ senders }),
    },
  );
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<ChannelSenders>;
}

/** Create or update an agent's Email channel config, including the enable
 *  flag (`PUT …/channels/email`). Enabling here atomically provisions the
 *  binding's dedicated bridge thread server-side. Throws `ApiError` (400)
 *  on invalid config (e.g. malformed address, zero port) or 404 for an
 *  unknown agent. */
export async function upsertEmailChannel(agentId: string, config: EmailChannelConfig): Promise<ChannelStatus> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/email`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(config),
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<ChannelStatus>;
}

/** Set (or replace) the password for an agent's Email channel
 *  (`PUT …/channels/email/secret`). Write-only: the password is never
 *  returned. Throws `ApiError` (400) on an empty password, or 404 for an
 *  unknown agent. */
export async function setEmailChannelSecret(agentId: string, password: string): Promise<ChannelStatus> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/email/secret`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password }),
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<ChannelStatus>;
}

/** Remove an agent's Email channel — clears its config, stored secret, and
 *  bridge thread linkage (`DELETE …/channels/email`). Resolves with no body
 *  on success (204); idempotent when no Email binding exists. */
export async function deleteEmailChannel(agentId: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/email`, { method: "DELETE" });
  if (!res.ok) return throwApiError(res);
}

/** Mirrors `ao_protocol::agent::ThreadFollowMode` — how long a thread the
 *  bot was mentioned in keeps responding without a fresh mention. */
export type ThreadFollowMode = "one_shot" | "sticky_decay" | "always";

/** Non-secret Discord binding config (`ChannelKindConfig::Discord`, minus
 *  the bot token, which only ever travels through `setDiscordChannelSecret`). */
export interface DiscordChannelConfig {
  allowed_users: string[];
  allowed_roles: string[];
  allowed_channels: string[];
  dm_role_auth_guild: string | null;
  require_mention: boolean;
  thread_follow: ThreadFollowMode;
  thread_idle_timeout_minutes: number;
  thread_message_budget: number;
  backfill_limit: number;
  enabled: boolean;
}

/** Create or update an agent's Discord channel config, including the enable
 *  flag (`PUT …/channels/discord`). Enabling here atomically provisions the
 *  binding's dedicated bridge thread server-side. Throws `ApiError` (400)
 *  on invalid config (e.g. blank `dm_role_auth_guild`) or 404 for an
 *  unknown agent. */
export async function upsertDiscordChannel(agentId: string, config: DiscordChannelConfig): Promise<ChannelStatus> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/discord`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(config),
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<ChannelStatus>;
}

/** Set (or replace) the bot token for an agent's Discord channel
 *  (`PUT …/channels/discord/secret`). Write-only: the token is never
 *  returned. Throws `ApiError` (400) on an empty token, or 404 for an
 *  unknown agent. */
export async function setDiscordChannelSecret(agentId: string, botToken: string): Promise<ChannelStatus> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/discord/secret`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ bot_token: botToken }),
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<ChannelStatus>;
}

/** Remove an agent's Discord channel — clears its config, stored bot token,
 *  and bridge thread linkage (`DELETE …/channels/discord`). Resolves with
 *  no body on success (204); idempotent when no Discord binding exists. */
export async function deleteDiscordChannel(agentId: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/discord`, { method: "DELETE" });
  if (!res.ok) return throwApiError(res);
}

/** Mirrors `ao_protocol::agent::SlackConversationMode` — how a Slack
 *  conversation (a DM, a channel `@mention` thread, or a reply inside one)
 *  maps onto a Launchpad bridge thread. Only one variant exists today; kept
 *  as a string union (not a boolean) so a future mode is additive. */
export type SlackConversationMode = "per_conversation";

/** Non-secret Slack binding config (`ChannelKindConfig::Slack`, minus the
 *  bot/app tokens, which only ever travel through `setSlackChannelSecret`). */
export interface SlackChannelConfig {
  allowed_users: string[];
  allowed_channels: string[];
  conversation_mode: SlackConversationMode;
  enabled: boolean;
}

/** Create or update an agent's Slack channel config, including the enable
 *  flag (`PUT …/channels/slack`). Enabling here atomically provisions the
 *  binding's dedicated bridge thread server-side. Throws `ApiError` (400)
 *  on invalid config or 404 for an unknown agent. */
export async function upsertSlackChannel(agentId: string, config: SlackChannelConfig): Promise<ChannelStatus> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/slack`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(config),
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<ChannelStatus>;
}

/** Set (or replace) both Slack tokens in one request
 *  (`PUT …/channels/slack/secret`). Write-only: neither token is ever
 *  returned. Throws `ApiError` (400) on an empty or wrong-prefixed token
 *  (e.g. the bot and app tokens swapped), or 404 for an unknown agent. */
export async function setSlackChannelSecret(agentId: string, botToken: string, appToken: string): Promise<ChannelStatus> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/slack/secret`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ bot_token: botToken, app_token: appToken }),
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<ChannelStatus>;
}

/** Remove an agent's Slack channel — clears its config, both stored tokens,
 *  and bridge thread linkage (`DELETE …/channels/slack`). Resolves with no
 *  body on success (204); idempotent when no Slack binding exists. Does
 *  *not* delete the workspace-level connection record the binding may point
 *  at. */
export async function deleteSlackChannel(agentId: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/slack`, { method: "DELETE" });
  if (!res.ok) return throwApiError(res);
}

export interface SlackManifestResponse {
  /** Full pasteable text — a leading comment block plus the YAML manifest
   *  itself, ready for Slack's "Create app → From an app manifest" flow. */
  manifest_yaml: string;
}

/** Fetch a prefilled Slack app manifest for this agent
 *  (`GET …/channels/slack/manifest`). Read-only and side-effect free — it
 *  needs only the agent's name and works before any Slack binding exists. */
export async function getSlackManifest(agentId: string): Promise<SlackManifestResponse> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/slack/manifest`);
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<SlackManifestResponse>;
}

/** Whether a Test Connection check failed because Slack was never reached
 *  (`"network"`) or because Slack reached and rejected the request
 *  (`"auth"`) — mirrors `ao_protocol::slack_test_connection::SlackFailureKind`. */
export type SlackFailureKind = "network" | "auth";

export interface SlackCheckFailure {
  kind: SlackFailureKind;
  /** Human-readable detail. Never contains a token. */
  message: string;
}

/** Pass/fail outcome of a single Test Connection check. `failure` is `null`
 *  exactly when `passed` is `true`. */
export interface SlackCheckOutcome {
  passed: boolean;
  failure: SlackCheckFailure | null;
}

/** Non-secret identity captured from a successful `auth.test` call. */
export interface SlackIdentitySummary {
  team_name: string;
  team_id: string;
  bot_handle: string;
  bot_user_id: string;
}

/** One required bot scope's grant status — a flat list rather than a single
 *  pass/fail so a user missing one of several scopes can see exactly which
 *  one to add. */
export interface SlackScopeGrant {
  scope: string;
  granted: boolean;
}

/** The full per-check result of one Test Connection run
 *  (`ao_protocol::slack_test_connection::SlackTestConnectionReport`). Never
 *  carries token material — only identity that was already safe to display
 *  and per-check pass/fail outcomes. */
export interface SlackTestConnectionReport {
  auth_check: SlackCheckOutcome;
  /** `null` when `auth_check.passed` is `false` — there is nothing to report. */
  identity: SlackIdentitySummary | null;
  /** Every required bot scope, each marked granted or not. Populated even
   *  when `auth_check` failed (all scopes read as not-granted). */
  scopes: SlackScopeGrant[];
  connections_open_check: SlackCheckOutcome;
}

/** Run Slack's Test Connection check (`auth.test`, a per-scope diff, and an
 *  `apps.connections.open` handshake) against the two already-stored tokens
 *  (`POST …/channels/slack/test-connection`). Throws `ApiError` (400) if
 *  either token hasn't been stored yet, or 404 for an unknown agent. */
export async function testSlackConnection(agentId: string): Promise<SlackTestConnectionReport> {
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/channels/slack/test-connection`, {
    method: "POST",
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<SlackTestConnectionReport>;
}

/** Build the SSE stream URL for an agent. */
export function streamUrl(agentId: string): string {
  return `${BASE_URL}/agents/${encodeURIComponent(agentId)}/stream`;
}

/** Build the SSE stream URL for per-task subagent run events on the
 *  tasklist-scoped channel, isolated from the parent chat. */
export function agentTasklistStreamUrl(agentId: string, tasklistId: string): string {
  return `${BASE_URL}/agents/${encodeURIComponent(agentId)}/tasklists/${encodeURIComponent(tasklistId)}/stream`;
}

/** Build the global system SSE stream URL (unfiltered event bus). */
export function systemStreamUrl(): string {
  return `${BASE_URL}/system/stream`;
}

// ---------------------------------------------------------------------------
// Attachment API functions
// ---------------------------------------------------------------------------

/** Upload a file attachment for an agent. */
export async function uploadAttachment(agentId: string, file: File): Promise<Attachment> {
  const formData = new FormData();
  formData.append("file", file);
  const res = await fetch(`${BASE_URL}/agents/${encodeURIComponent(agentId)}/attachments`, {
    method: "POST",
    body: formData,
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
  return res.json() as Promise<Attachment>;
}

/** Add a folder reference for an agent. */
export async function addFolderReference(agentId: string, path: string): Promise<Attachment> {
  return fetchJson<Attachment>(`/agents/${encodeURIComponent(agentId)}/attachments/folder`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ path }),
  });
}

/** Delete an attachment for an agent. */
export async function deleteAttachment(agentId: string, attachmentId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/attachments/${encodeURIComponent(attachmentId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Fetch attachment metadata (info) without downloading the file bytes. */
export async function getAttachmentInfo(agentId: string, attachmentId: string): Promise<Attachment> {
  return fetchJson<Attachment>(
    `/agents/${encodeURIComponent(agentId)}/attachments/${encodeURIComponent(attachmentId)}/info`,
  );
}

/** List all attachments for an agent. */
export async function listAttachments(agentId: string): Promise<Attachment[]> {
  return fetchJson<Attachment[]>(`/agents/${encodeURIComponent(agentId)}/attachments`);
}

/** Returns the URL path for serving an attachment. */
export function getAttachmentUrl(agentId: string, attachmentId: string): string {
  return `${BASE_URL}/agents/${encodeURIComponent(agentId)}/attachments/${encodeURIComponent(attachmentId)}`;
}

/** Check whether a server-side attachment still exists. Returns `true` if the
 *  info endpoint responds 2xx, `false` on 404 (the uncommitted-asset GC deleted
 *  it), and `null` if the check could not be performed (network error, etc.)
 *  so callers can avoid falsely marking attachments as expired. */
export async function verifyAttachmentExists(
  agentId: string,
  attachmentId: string,
): Promise<boolean | null> {
  try {
    const res = await fetch(
      `${BASE_URL}/agents/${encodeURIComponent(agentId)}/attachments/${encodeURIComponent(attachmentId)}/info`,
    );
    if (res.ok) return true;
    if (res.status === 404) return false;
    return null;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Artifact API functions
// ---------------------------------------------------------------------------

/** List all artifact records for an agent (metadata only, no payload). */
export async function listArtifacts(agentId: string): Promise<Artifact[]> {
  return fetchJson<Artifact[]>(`/agents/${encodeURIComponent(agentId)}/artifacts`);
}

/** Fetch a single artifact's record together with its current payload. */
export async function getArtifact(agentId: string, artifactId: string): Promise<ArtifactWithPayload> {
  return fetchJson<ArtifactWithPayload>(
    `/agents/${encodeURIComponent(agentId)}/artifacts/${encodeURIComponent(artifactId)}`,
  );
}

/** Create a new artifact for an agent. */
export async function createArtifact(agentId: string, request: CreateArtifactRequest): Promise<Artifact> {
  return fetchJson<Artifact>(`/agents/${encodeURIComponent(agentId)}/artifacts`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  });
}

export interface RegenerateArtifactResponse {
  task_id: string;
}

/** Trigger whole-artifact regeneration: the server replays the artifact's
 *  `origin_intent.refresh_prompt` through a fresh background agent that
 *  overwrites the artifact in place (`ArtifactWrite` with the same id).
 *  Returns immediately (202) with the spawned agent's id — the artifact
 *  record itself only updates once that agent's write lands, so callers
 *  poll {@link getArtifact} for completion (see `useArtifactRegen`). Only
 *  valid when `refresh_intent === "whole_artifact"` and `origin_intent` is
 *  set; the server 409s otherwise. */
export async function regenerateArtifact(
  agentId: string,
  artifactId: string,
): Promise<RegenerateArtifactResponse> {
  return fetchJson<RegenerateArtifactResponse>(
    `/agents/${encodeURIComponent(agentId)}/artifacts/${encodeURIComponent(artifactId)}/regenerate`,
    { method: "POST" },
  );
}

/** Revert an artifact's most recent edit synchronously — no background
 *  agent, no polling. Returns the updated artifact record (same shape as
 *  {@link getArtifact} minus `payload`) with a fresh `undo_available` flag;
 *  callers still re-fetch via {@link getArtifact} to pick up the restored
 *  payload through the shared fresh-by-id render path rather than rendering
 *  off this response directly. Throws `ApiError` with `status === 409` when
 *  there is no prior edit left to revert to. */
export async function undoArtifact(agentId: string, artifactId: string): Promise<Artifact> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/artifacts/${encodeURIComponent(artifactId)}/undo`,
    { method: "POST" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let message = body || `Request failed with status ${res.status}`;
    try {
      const parsed = JSON.parse(body);
      if (parsed && typeof parsed.error === "string") message = parsed.error;
    } catch {
      // body wasn't JSON — fall through with raw text
    }
    throw new ApiError(res.status, message);
  }
  return res.json() as Promise<Artifact>;
}

/** One turn of the artifact chat mini-thread's transcript, sent to seed the
 *  chat-adjust subagent's context. Mirrors `ChatTranscriptTurn`
 *  (`crates/ao-server/src/routes/artifacts.rs`). */
export interface ChatTranscriptTurn {
  role: "user" | "assistant";
  content: string;
}

export interface ChatArtifactResponse {
  task_id: string;
}

/** Send one chat-to-adjust message for an artifact: the server seeds a
 *  background subagent with `message` plus the artifact's intent ledger and
 *  `transcript`, which edits the artifact in place via `ArtifactWrite`.
 *  Returns immediately (202) with the spawned agent's id — same
 *  trigger-and-poll contract as {@link regenerateArtifact} (see
 *  `useArtifactRegen`). Once the poll observes the artifact change, the
 *  agent's reply is the last entry of the refetched artifact's
 *  `intent_ledger` (`intent_note`), not part of this response. */
export async function chatArtifact(
  agentId: string,
  artifactId: string,
  message: string,
  transcript: ChatTranscriptTurn[] = [],
): Promise<ChatArtifactResponse> {
  return fetchJson<ChatArtifactResponse>(
    `/agents/${encodeURIComponent(agentId)}/artifacts/${encodeURIComponent(artifactId)}/chat`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ message, transcript }),
    },
  );
}

export interface ArtifactTaskStatusResponse {
  status: "running" | "completed" | "failed" | "unknown";
  error: string | null;
}

/** Poll the in-memory status of a background subagent run kicked off by
 *  {@link regenerateArtifact} or {@link chatArtifact}. Always HTTP 200, even
 *  for a bogus/unknown `taskId` — that case (and one queried before the run
 *  has reached `mark_running`) comes back as `status: "unknown"`, which
 *  callers must treat exactly like `"running"`, never as an error. A
 *  cancelled run collapses server-side into `"failed"` with
 *  `error: "cancelled"` — there is no separate cancelled status. */
export async function getArtifactTaskStatus(
  agentId: string,
  artifactId: string,
  taskId: string,
): Promise<ArtifactTaskStatusResponse> {
  return fetchJson<ArtifactTaskStatusResponse>(
    `/agents/${encodeURIComponent(agentId)}/artifacts/${encodeURIComponent(artifactId)}/task/${encodeURIComponent(taskId)}/status`,
  );
}

/** One entry of an artifact chat mini-thread's durable server transcript
 *  (`GET .../artifacts/{id}/chat`). Narrower than the general
 *  {@link TranscriptEntry} — this endpoint's `role` always serializes as a
 *  bare `"user"` / `"assistant"` string (never the `{ agent }` / `{ task_id
 *  }` shapes agent-to-agent transcripts can carry). `hidden_from_user` is
 *  omitted by the server when `false` (wire-compat default), so treat
 *  `undefined` as `false`, same convention as `TranscriptEntry`. */
export interface ArtifactChatEntry {
  ts: string;
  role: "user" | "assistant";
  content: string;
  event_type: string;
  metadata: Record<string, unknown> | null;
  hidden_from_user?: boolean;
}

export interface ArtifactChatGetResponse {
  entries: ArtifactChatEntry[];
  cursor: PaginationCursor | null;
}

/** Fetch the durable server-side chat transcript for an artifact's
 *  mini-thread — written to by {@link chatArtifact} (user turns) and the
 *  background subagent's completion (assistant replies). Relies on the
 *  server default of the `last` most-recent entries (50) rather than passing
 *  the query param explicitly. An artifact that's never been chatted with
 *  returns `{ entries: [], cursor: null }`, not an error. */
export async function getArtifactChat(agentId: string, artifactId: string): Promise<ArtifactChatGetResponse> {
  return fetchJson<ArtifactChatGetResponse>(
    `/agents/${encodeURIComponent(agentId)}/artifacts/${encodeURIComponent(artifactId)}/chat`,
  );
}

/** Delete an artifact for an agent. */
export async function deleteArtifact(agentId: string, artifactId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/artifacts/${encodeURIComponent(artifactId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Save-to-Assets toggle: flips an artifact's `pinned` flag. Metadata-only. */
export async function setArtifactPinned(agentId: string, artifactId: string, pinned: boolean): Promise<Artifact> {
  return fetchJson<Artifact>(
    `/agents/${encodeURIComponent(agentId)}/artifacts/${encodeURIComponent(artifactId)}/pin`,
    {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ pinned }),
    },
  );
}

/** Every pinned artifact across every agent, for the global Assets page. */
export async function listPinnedArtifacts(): Promise<PinnedArtifact[]> {
  return fetchJson<PinnedArtifact[]>(`/artifacts/pinned`);
}

/** Files a pinned artifact under a group, or clears it back to ungrouped
 *  with `groupId: null`. */
export async function setArtifactGroup(
  agentId: string,
  artifactId: string,
  groupId: string | null,
): Promise<Artifact> {
  return fetchJson<Artifact>(
    `/agents/${encodeURIComponent(agentId)}/artifacts/${encodeURIComponent(artifactId)}/group`,
    {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ group_id: groupId }),
    },
  );
}

/** Every artifact group, for the Assets sidebar and the group-picker modal. */
export async function listArtifactGroups(): Promise<ArtifactGroup[]> {
  return fetchJson<ArtifactGroup[]>(`/artifact-groups`);
}

/** Create a new artifact group. */
export async function createArtifactGroup(name: string): Promise<ArtifactGroup> {
  return fetchJson<ArtifactGroup>(`/artifact-groups`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });
}

/** Delete an artifact group; unfiles every artifact that referenced it. */
export async function deleteArtifactGroup(groupId: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/artifact-groups/${encodeURIComponent(groupId)}`, {
    method: "DELETE",
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

// ---------------------------------------------------------------------------
// Memory API functions
// ---------------------------------------------------------------------------

/** Fetch all memories for a specific agent. */
export async function getMemories(agentId: string): Promise<MemoryEntry[]> {
  return fetchJson<MemoryEntry[]>(`/agents/${encodeURIComponent(agentId)}/memories`);
}

/** Add a new memory for a specific agent. */
export async function addMemory(agentId: string, content: string): Promise<MemoryEntry> {
  return fetchJson<MemoryEntry>(`/agents/${encodeURIComponent(agentId)}/memories`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ content }),
  });
}

/** Delete a memory for a specific agent. */
export async function deleteMemory(agentId: string, memoryId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/memories/${encodeURIComponent(memoryId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Fetch all global memories. */
export async function getGlobalMemories(): Promise<MemoryEntry[]> {
  return fetchJson<MemoryEntry[]>("/memories/global");
}

/** Add a new global memory. */
export async function addGlobalMemory(content: string): Promise<MemoryEntry> {
  return fetchJson<MemoryEntry>("/memories/global", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ content }),
  });
}

/** Delete a global memory. */
export async function deleteGlobalMemory(memoryId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/memories/global/${encodeURIComponent(memoryId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/**
 * Fetch project-scoped memories for the repo the given agent operates in.
 * The server resolves the project hash from the agent's working directory.
 */
export async function getProjectMemories(agentId: string): Promise<MemoryEntry[]> {
  return fetchJson<MemoryEntry[]>(
    `/agents/${encodeURIComponent(agentId)}/memories/project`,
  );
}

/** Add a new project-scoped memory for the agent's repo. */
export async function addProjectMemory(agentId: string, content: string): Promise<MemoryEntry> {
  return fetchJson<MemoryEntry>(
    `/agents/${encodeURIComponent(agentId)}/memories/project`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ content }),
    },
  );
}

/** Delete a project-scoped memory for the agent's repo. */
export async function deleteProjectMemory(agentId: string, memoryId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/memories/project/${encodeURIComponent(memoryId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Fetch all memories pinned to a specific thread. */
export async function getThreadMemories(threadId: string): Promise<MemoryEntry[]> {
  return fetchJson<MemoryEntry[]>(`/memories/thread/${encodeURIComponent(threadId)}`);
}

/** Add a new memory pinned to a specific thread. */
export async function addThreadMemory(threadId: string, content: string): Promise<MemoryEntry> {
  return fetchJson<MemoryEntry>(`/memories/thread/${encodeURIComponent(threadId)}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ content }),
  });
}

/** Delete a memory pinned to a specific thread. */
export async function deleteThreadMemory(threadId: string, memoryId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/memories/thread/${encodeURIComponent(threadId)}/${encodeURIComponent(memoryId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

// ---------------------------------------------------------------------------
// Memory review queue + undo (self-improving memory)
// ---------------------------------------------------------------------------

/** Fetch an agent's pending staged self-improvement candidates (the review queue). */
export async function getMemoryReviewQueue(agentId: string): Promise<ReflectionCandidate[]> {
  return fetchJson<ReflectionCandidate[]>(
    `/agents/${encodeURIComponent(agentId)}/memories/review`,
  );
}

/** Per-agent rollup used by the Settings → Memories agent list column
 *  (pending-review badge + "recently active" sort). */
export interface AgentMemorySummary {
  pending: number;
  last_memory_at: string | null;
}

/**
 * Fetch every agent's review-queue count + most recent self-authored memory
 * timestamp in one request, keyed by agent id. Backs the agent list column's
 * badge/sort without fanning out two requests per agent from the browser —
 * that fan-out approach hits browsers' ~6-connections-per-origin cap once
 * the agent count reaches dozens.
 */
export async function getAgentMemorySummaries(): Promise<Record<string, AgentMemorySummary>> {
  return fetchJson<Record<string, AgentMemorySummary>>("/agents/memories/summary");
}

/** One of the actions a human can take on a staged candidate in the review queue. */
export type MemoryReviewAction = "keep" | "edit" | "forget" | "pin";

/**
 * Act on a single staged candidate. `content` is required for `edit` and
 * ignored otherwise. Returns the raw server outcome, whose shape varies by
 * action (e.g. a `memory_id` for keep/edit/pin, `{ rejected: true }` for forget).
 */
export async function actOnMemoryReviewCandidate(
  agentId: string,
  candidateId: string,
  action: MemoryReviewAction,
  content?: string,
): Promise<unknown> {
  return fetchJson<unknown>(
    `/agents/${encodeURIComponent(agentId)}/memories/review/${encodeURIComponent(candidateId)}`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(action === "edit" ? { action, content } : { action }),
    },
  );
}

export interface UndoMemoryOutcome {
  memory_id: string;
  [key: string]: unknown;
}

/**
 * Reverse a live memory write by the id of the entry it produced — whether it
 * went live via the trust gate's auto-confirm tier or through a keep/edit/pin
 * review action. `scope` defaults to the agent scope (the only scope the
 * reflection pass writes today); pass `scopeKey` for project-scoped undo.
 */
export async function undoMemoryWrite(
  agentId: string,
  memoryId: string,
  scope: MemoryScope = "Agent",
  scopeKey?: string,
): Promise<UndoMemoryOutcome> {
  return fetchJson<UndoMemoryOutcome>(
    `/agents/${encodeURIComponent(agentId)}/memories/undo`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ scope, scope_key: scopeKey ?? null, memory_id: memoryId }),
    },
  );
}

// ---------------------------------------------------------------------------
// Skill review queue (self-improving skills — the skill-side sibling of the
// memory review queue above). Surfaces parked, distilled skills a human hasn't
// approved yet, plus raw single observations a human can promote into one.
// ---------------------------------------------------------------------------

/** A parked skill awaiting human review — not model-invokable until
 *  accepted/edited-live. Mirrors the backend `ParkedSkillCandidate`. */
export interface ParkedSkillCandidate {
  name: string;
  description: string;
  body: string;
  /** Which writer parked it: the distillation pipeline, or an agent writing a
   *  skill on request via SkillRegister. */
  origin: "distilled" | "user_authored";
  /** Ids of the raw observations this skill was distilled from. Empty for a
   *  `user_authored` skill, which was written directly rather than generalized
   *  from observations. */
  distilled_from: string[];
  created_at: string;
}

/** A raw, still-pending skill observation eligible for manual promotion into a
 *  parked distilled skill. Mirrors the backend `SkillObservation`. */
export interface SkillObservation {
  id: string;
  content: string;
  source_thread_id: string;
  created_at: string;
}

/** The full skill review queue for one agent. */
export interface SkillReviewQueue {
  candidates: ParkedSkillCandidate[];
  observations: SkillObservation[];
}

/** Fetch an agent's parked distilled skills + promotable raw observations. */
export async function getSkillReviewQueue(agentId: string): Promise<SkillReviewQueue> {
  return fetchJson<SkillReviewQueue>(
    `/agents/${encodeURIComponent(agentId)}/skills/review`,
  );
}

/** One of the actions a human can take on a parked distilled skill. */
export type SkillReviewAction = "accept" | "edit" | "reject";

/**
 * Act on a single parked distilled skill by name.
 *  - `accept` → flips the skill live (model-invokable) as-is.
 *  - `edit`   → requires a non-empty `body`; `description` is rewritten too when
 *               given. Goes live unless `keep_parked` is true (rewrite but stay
 *               parked).
 *  - `reject` → deletes the parked skill entirely.
 * Returns the raw server outcome, whose shape varies by action.
 */
export async function actOnSkillReview(
  agentId: string,
  skillName: string,
  opts: {
    action: SkillReviewAction;
    body?: string;
    description?: string;
    keep_parked?: boolean;
  },
): Promise<unknown> {
  return fetchJson<unknown>(
    `/agents/${encodeURIComponent(agentId)}/skills/review/${encodeURIComponent(skillName)}`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      // `JSON.stringify` drops undefined-valued keys, so an accept/reject sends
      // just `{ action }` while edit carries the optional body/description/keep.
      body: JSON.stringify({
        action: opts.action,
        body: opts.body,
        description: opts.description,
        keep_parked: opts.keep_parked,
      }),
    },
  );
}

/**
 * Promote a raw pending skill observation into a parked distilled skill.
 * Returns the newly-parked candidate — use the returned `name` for any
 * subsequent accept/edit/reject, since sanitization/collision-breaking may
 * have changed it from whatever the observation displayed.
 */
export async function promoteSkillObservation(
  agentId: string,
  candidateId: string,
): Promise<ParkedSkillCandidate> {
  // Server-side LLM round-trip: the candidate observation is rewritten into
  // a skill by a provider call before this responds.
  return fetchJson<ParkedSkillCandidate>(
    `/agents/${encodeURIComponent(agentId)}/skills/review/promote`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ candidate_id: candidateId }),
    },
    EXTERNAL_WORK_TIMEOUT_MS,
  );
}

// ---------------------------------------------------------------------------
// Bookmark API functions
// ---------------------------------------------------------------------------

/** Fetch all bookmarks for a specific agent. */
export async function getBookmarks(agentId: string): Promise<BookmarkEntry[]> {
  return fetchJson<BookmarkEntry[]>(`/agents/${encodeURIComponent(agentId)}/bookmarks`);
}

/** Add a bookmark for a specific agent message. */
export async function addBookmark(
  agentId: string,
  messagTs: string,
  messageContent: string,
  messageRole: string | { agent: string } | { task_id: string },
): Promise<BookmarkEntry> {
  return fetchJson<BookmarkEntry>(`/agents/${encodeURIComponent(agentId)}/bookmarks`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ message_ts: messagTs, message_content: messageContent, message_role: messageRole }),
  });
}

/** Delete a bookmark for a specific agent. */
export async function deleteBookmark(agentId: string, bookmarkId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/bookmarks/${encodeURIComponent(bookmarkId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

// ---------------------------------------------------------------------------
// Scope-aware tasklist API (dispatches to /teams or /projects routes)
// ---------------------------------------------------------------------------

function tasklistBaseUrl(scope: TasklistScope): string {
  const kind = scope.kind === "team" ? "teams" : "projects";
  return `/${kind}/${encodeURIComponent(scope.id)}/tasklists`;
}

export async function listTasklistsForScope(scope: TasklistScope): Promise<ListTasklistsResponse> {
  return fetchJson<ListTasklistsResponse>(tasklistBaseUrl(scope));
}

export async function getTasklistForScope(scope: TasklistScope, tasklistId: string): Promise<Tasklist> {
  return fetchJson<Tasklist>(`${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}`);
}

export async function appendTaskForScope(scope: TasklistScope, tasklistId: string, req: AppendTaskRequest): Promise<Tasklist> {
  return fetchJson<Tasklist>(`${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/tasks`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
}

export async function addTaskCommentForScope(scope: TasklistScope, tasklistId: string, taskId: string, req: AddTaskCommentRequest): Promise<TaskComment> {
  return fetchJson<TaskComment>(
    `${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/tasks/${encodeURIComponent(taskId)}/comments`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(req),
    },
  );
}

export async function setTasklistStatusForScope(scope: TasklistScope, tasklistId: string, status: "active" | "paused"): Promise<Tasklist> {
  return fetchJson<Tasklist>(`${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/status`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ status }),
  });
}

export async function continueTasklistForScope(scope: TasklistScope, tasklistId: string): Promise<Tasklist> {
  return fetchJson<Tasklist>(`${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/continue`, { method: "POST" });
}

export async function skipTaskForScope(scope: TasklistScope, tasklistId: string, taskId: string): Promise<Tasklist> {
  return fetchJson<Tasklist>(
    `${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/tasks/${encodeURIComponent(taskId)}/skip`,
    { method: "POST" },
  );
}

/** Cancel a single in-flight task: flips it to "stopped" and kills its
 *  in-flight run. Non-terminal — re-queue it with {@link resumeTaskForScope}.
 *  Currently only the /projects routes expose this; team scopes will 404. */
export async function stopTaskForScope(scope: TasklistScope, tasklistId: string, taskId: string): Promise<Tasklist> {
  return fetchJson<Tasklist>(
    `${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/tasks/${encodeURIComponent(taskId)}/stop`,
    { method: "POST" },
  );
}

/** Re-queue a stopped task: flips it back to "pending" so the feeder
 *  re-dispatches it. Counterpart of {@link stopTaskForScope}. */
export async function resumeTaskForScope(scope: TasklistScope, tasklistId: string, taskId: string): Promise<Tasklist> {
  return fetchJson<Tasklist>(
    `${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/tasks/${encodeURIComponent(taskId)}/resume`,
    { method: "POST" },
  );
}

export async function discardTasklistForScope(scope: TasklistScope, tasklistId: string): Promise<Tasklist> {
  return fetchJson<Tasklist>(`${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/discard`, { method: "POST" });
}

export async function replayTasklistForScope(scope: TasklistScope, tasklistId: string): Promise<Tasklist> {
  return fetchJson<Tasklist>(`${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/replay`, { method: "POST" });
}

export function getTasklistOutputUrlForScope(scope: TasklistScope, tasklistId: string, filename: string): string {
  const encodedFilename = filename.split("/").map((s) => encodeURIComponent(s)).join("/");
  return `${BASE_URL}${tasklistBaseUrl(scope)}/${encodeURIComponent(tasklistId)}/outputs/${encodedFilename}`;
}

export async function getTasklistOutputForScope(scope: TasklistScope, tasklistId: string, filename: string): Promise<string> {
  const res = await fetch(getTasklistOutputUrlForScope(scope, tasklistId, filename));
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
  return res.text();
}

// ---------------------------------------------------------------------------
// User Preferences API functions
// ---------------------------------------------------------------------------

export interface UserPreferences {
  full_name: string | null;
  preferred_name: string | null;
  timezone: string | null;
  language: string | null;
  locale: string | null;
  max_sleep_guard_hours?: number | null;
  prevent_sleep_during_workflows?: boolean;
  prevent_sleep_during_agent_runs?: boolean;
  prevent_sleep_during_tasklists?: boolean;
  keep_display_awake?: boolean;
}

export interface PreferencesStatus {
  configured: boolean;
}

/** Fetch current user preferences. */
export async function getPreferences(): Promise<UserPreferences> {
  return fetchJson<UserPreferences>("/preferences");
}

/** Update user preferences. */
export async function putPreferences(prefs: UserPreferences): Promise<UserPreferences> {
  return fetchJson<UserPreferences>("/preferences", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(prefs),
  });
}

/** Check if user preferences are configured. */
export async function getPreferencesStatus(): Promise<PreferencesStatus> {
  return fetchJson<PreferencesStatus>("/preferences/status");
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

/** Fixed palette a workspace's `color` is derived from (or picked out of),
 *  mirrored verbatim from `WORKSPACE_COLOR_PALETTE` in
 *  `crates/ao-protocol/src/workspaces.rs`. Keep both lists in sync by hand —
 *  the picker must offer exactly the same choices the backend can derive. */
export const WORKSPACE_COLOR_PALETTE = [
  "#EF4444",
  "#F97316",
  "#EAB308",
  "#22C55E",
  "#14B8A6",
  "#3B82F6",
  "#8B5CF6",
  "#EC4899",
] as const;

export interface WorkspaceEntry {
  id: string;
  name: string;
  path: string;
  /** Hex color from `WORKSPACE_COLOR_PALETTE`. Always populated by the
   *  server — legacy/absent values are backfilled server-side (deterministically
   *  derived from `id`) before this ever reaches the client. */
  color: string;
  /** User-chosen emoji, genuinely optional. Absent, `null`, or
   *  empty-after-trim all mean "unset" — the avatar falls back to a letter
   *  tile in that case (see `WorkspaceAvatar`). A non-empty string is a
   *  deliberate opt-in; there is no server-side default emoji anymore. */
  emoji?: string | null;
}

export interface WorkspaceListResponse {
  workspaces: WorkspaceEntry[];
  active: string;
}

/** Result of `createWorkspace` — the registered entry plus which of the two
 *  paths the backend took for the submitted folder. */
export interface CreateWorkspaceResult extends WorkspaceEntry {
  /** `true` when an existing Launchpad data-root folder was registered
   *  as-is (adopted, untouched); `false` when a new directory was scaffolded
   *  from scratch. Callers use this to show distinct confirmation copy — the
   *  two outcomes leave very different things on disk. */
  adopted: boolean;
}

/** List every registered workspace and which one is active. Note the
 *  `active` id names an entry in the on-disk registry
 *  (`~/.launchpad_studio/workspaces.json`, one fixed path shared by every
 *  process on the machine) — it does not account for a
 *  `LAUNCHPAD_STUDIO_DATA_DIR` env var override, which always outranks the
 *  registry when resolving which data root a given process is actually
 *  reading and writing. */
export async function getWorkspaces(): Promise<WorkspaceListResponse> {
  return fetchJson<WorkspaceListResponse>("/workspaces");
}

/** Which precedence branch produced `ActiveWorkspaceResponse.path`:
 *  - `"env_override"` — `LAUNCHPAD_STUDIO_DATA_DIR` was INHERITED from the
 *    environment this process was launched with — a deliberate operator
 *    pin. The registry was never consulted and switching workspaces has no
 *    effect until that env var is unset and the app relaunched.
 *  - `"fallback"` — the same env var is set, but this process pinned it on
 *    ITSELF during startup crash recovery, after the workspace it actually
 *    resolved to failed to initialize (see `ActiveWorkspaceResponse.
 *    startup_fallback` for diagnostics). Unlike `"env_override"` this is not
 *    an operator instruction — it's the opposite: the switcher must stay
 *    fully interactive, since activating a different workspace is the only
 *    way out of this state.
 *  - `"registry"` — the registry's active entry was used; `name` is set.
 *  - `"home_default"` — no env override and no registry entry; the built-in
 *    default root under the user's home directory was used. */
export type RootProvenance = "env_override" | "fallback" | "registry" | "home_default";

/** Diagnostics for a `"fallback"` `RootProvenance` — the workspace that
 *  failed to open, the default root the app fell back to instead, and the
 *  underlying error's `Display` text. Mirrors
 *  `ao_protocol::data_root::StartupFallback` on the wire. */
export interface StartupFallback {
  failed_root: string;
  fallback_root: string;
  error: string;
}

export interface ActiveWorkspaceResponse {
  path: string;
  provenance: RootProvenance;
  /** Registry display name. Only ever non-null when `provenance ===
   *  "registry"` — looked up by matching the resolved path against a
   *  freshly-loaded registry, so it can't mismatch during the window between
   *  activating a workspace and the restart that switch requires. */
  name: string | null;
  /** Populated when this process's startup couldn't initialize the
   *  workspace it actually resolved to and fell back to the default data
   *  root instead — `null` in the normal case, including whenever
   *  `provenance !== "fallback"`. See `StartupFallback`. */
  startup_fallback: StartupFallback | null;
}

/** The data root this process is *actually* reading and writing right now,
 *  plus which precedence branch (env override / registry / home default)
 *  produced it. Unlike `getWorkspaces`, this reflects the real effective
 *  root — including a `LAUNCHPAD_STUDIO_DATA_DIR` override, which
 *  `GET /workspaces` has no way to represent since it only ever reads the
 *  on-disk registry. This is the only source of truth for "what workspace is
 *  this window using" — do not derive that from `getWorkspaces` output. */
export async function getActiveWorkspace(): Promise<ActiveWorkspaceResponse> {
  return fetchJson<ActiveWorkspaceResponse>("/workspaces/active");
}

/** Register a new workspace at `path`. If `path` doesn't exist or is an
 *  empty directory, its on-disk directory tree is scaffolded from scratch
 *  (`adopted: false` on the result). If `path` is already a Launchpad
 *  data-root folder, it's registered as-is, untouched (`adopted: true`).
 *  Does not activate it — call `activateWorkspace` (and restart) to switch.
 *  `color` is optional — omit (or pass `undefined`) to let the server derive
 *  one from the new id. `emoji` is optional and genuinely nullable — omit,
 *  or pass `null`/`undefined`, to leave the workspace with no emoji (it gets
 *  a letter avatar instead of a server-side default emoji); pass a non-empty
 *  string to opt in.
 *  Throws `ApiError` (400) for a relative path, a path collision with an
 *  already-registered workspace, or a path that exists, is non-empty, and
 *  isn't already a Launchpad workspace. */
export async function createWorkspace(
  name: string,
  path: string,
  color?: string,
  emoji?: string | null,
): Promise<CreateWorkspaceResult> {
  const res = await fetch(`${BASE_URL}/workspaces`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name, path, color, emoji }),
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<CreateWorkspaceResult>;
}

/** Rename a workspace. The path is immutable — duplicate to relocate.
 *  `color`/`emoji` are optional and independent of the rename: this is a
 *  partial update, not a replace, and `emoji` carries meaning through three
 *  distinct states rather than two:
 *  - omitted (`undefined`) — leave the existing emoji unchanged.
 *  - `null` — explicitly clear it (JSON-serialized as a real `null`, not
 *    dropped from the request body); the workspace falls back to a letter
 *    avatar.
 *  - a non-empty string — set it.
 *  Throws `ApiError` (404) for an unknown id. */
export async function renameWorkspace(
  id: string,
  name: string,
  color?: string,
  emoji?: string | null,
): Promise<WorkspaceEntry> {
  const res = await fetch(`${BASE_URL}/workspaces/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name, color, emoji }),
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<WorkspaceEntry>;
}

/** Deregister a workspace. Only removes the registry entry — files already
 *  on disk are left untouched. Throws `ApiError` (409) for the currently
 *  active workspace or the last remaining one; (404) for an unknown id. */
export async function deleteWorkspace(id: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/workspaces/${encodeURIComponent(id)}`, { method: "DELETE" });
  if (!res.ok) return throwApiError(res);
}

/** Point the registry's active pointer at `id` and persist it. This ONLY
 *  writes the pointer — the running process keeps reading/writing the data
 *  root it started with, so the switch has no effect until the whole app
 *  restarts. Callers must trigger a restart immediately after this resolves;
 *  never leave the pointer changed without following through, or the app
 *  silently keeps running against the previous workspace while the registry
 *  claims a different one is active. Throws `ApiError` (409) if another
 *  running Launchpad Studio process already has this workspace's data root
 *  open — the message names the pid so the user can go quit it. */
export async function activateWorkspace(id: string): Promise<WorkspaceListResponse> {
  const res = await fetch(`${BASE_URL}/workspaces/${encodeURIComponent(id)}/activate`, { method: "POST" });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<WorkspaceListResponse>;
}

/** Copy an existing workspace's on-disk tree to a new path and register the
 *  copy under a new name. Does not activate it. Throws `ApiError` (404) if
 *  the source id is unknown, (400) for the same path/name conditions as
 *  `createWorkspace` applied to the target, or if the source's on-disk path
 *  no longer exists. */
export async function duplicateWorkspace(id: string, name: string, path: string): Promise<WorkspaceEntry> {
  const res = await fetch(`${BASE_URL}/workspaces/${encodeURIComponent(id)}/duplicate`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name, path }),
  });
  if (!res.ok) return throwApiError(res);
  return res.json() as Promise<WorkspaceEntry>;
}

// ---------------------------------------------------------------------------
// System / Config
// ---------------------------------------------------------------------------

export interface AppConfigResponse {
  min_agent_watch_poll_interval_secs: number;
}

/** Server-computed runtime config (e.g. the effective `AgentWatch` poll-interval
 *  floor, which a demo/local override can lower below its shipped default). */
export async function getAppConfig(): Promise<AppConfigResponse> {
  return fetchJson<AppConfigResponse>("/system/config");
}

// ---------------------------------------------------------------------------
// System / Storage
// ---------------------------------------------------------------------------

export interface AgentStorageInfo {
  agent_id: string;
  asset_count: number;
  total_size_bytes: number;
}

export interface StorageResponse {
  total_assets: number;
  total_size_bytes: number;
  per_agent: AgentStorageInfo[];
}

export interface CleanupResponse {
  total_cleaned: number;
  total_freed_bytes: number;
  per_agent: { agent_id: string; cleaned_count: number; freed_bytes: number }[];
}

/** Get storage usage across all agents. */
export async function getStorageInfo(): Promise<StorageResponse> {
  // Walks the entire data root to sum file sizes — scales with total
  // workspace size, not with network conditions.
  return fetchJson<StorageResponse>("/system/storage", {}, BULK_FETCH_TIMEOUT_MS);
}

/** Trigger immediate cleanup of orphaned files. */
export async function triggerCleanup(): Promise<CleanupResponse> {
  // Sweeps and unlinks orphaned files across the whole data root — scales
  // with how much accumulated garbage the user has.
  return fetchJson<CleanupResponse>("/system/cleanup", { method: "POST" }, BULK_FETCH_TIMEOUT_MS);
}

// ---------------------------------------------------------------------------
// Agent command discovery
// ---------------------------------------------------------------------------

export interface AgentCommand {
  slug: string;
  name: string;
  description: string | null;
  source_type: string;
  scope: string;
}

/** Fetch discovered slash commands/skills for a CLI agent type. */
export async function getAgentCommands(
  command: string,
  workingDir?: string | null,
): Promise<AgentCommand[]> {
  const params = new URLSearchParams({ command });
  if (workingDir) params.set("working_dir", workingDir);
  const resp = await fetchJson<{ commands: AgentCommand[] }>(
    `/agents/commands?${params}`,
  );
  return resp.commands;
}

// ---------------------------------------------------------------------------
// Workflow API functions
// ---------------------------------------------------------------------------

/** Fetch all workflow summaries. */
export async function getWorkflows(): Promise<WorkflowSummary[]> {
  return fetchJson<WorkflowSummary[]>("/workflows");
}

/** Fetch full workflow definition by id. */
export async function getWorkflow(id: string): Promise<WorkflowDefinition> {
  return fetchJson<WorkflowDefinition>(`/workflows/${encodeURIComponent(id)}`);
}

/** Import a workflow from a local folder path. */
export async function importWorkflow(sourcePath: string): Promise<{ workflow_id: string; status: string }> {
  // Recursive copy of an arbitrary user-chosen workflow folder (phase assets
  // included) into the workspace.
  return fetchJson("/workflows/import", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ source_path: sourcePath }),
  }, BULK_FETCH_TIMEOUT_MS);
}

/** Clone an example workflow from inline file contents. */
export async function cloneExampleWorkflow(
  id: string,
  files: Record<string, string>,
): Promise<{ workflow_id: string; status: string }> {
  return fetchJson("/workflows/clone-example", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, files }),
  });
}

/** Re-scan workflows directory and return updated count. */
export async function refreshWorkflows(): Promise<RefreshResponse> {
  return fetchJson<RefreshResponse>("/workflows/refresh", { method: "POST" });
}

/** Create a new workflow task. */
export async function createTask(workflowId: string, body: CreateTaskRequest): Promise<CreateTaskResponse> {
  return fetchJson<CreateTaskResponse>(`/workflows/${encodeURIComponent(workflowId)}/tasks`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

/** Fetch all task summaries. When archived is true, returns only archived tasks. */
export async function getTasks(opts?: { archived?: boolean }): Promise<TaskSummary[]> {
  const params = opts?.archived ? "?archived=true" : "";
  return fetchJson<TaskSummary[]>(`/tasks${params}`);
}

/** Delete a task by id. */
export async function deleteTask(id: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/tasks/${encodeURIComponent(id)}`, { method: "DELETE" });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Archive a task by id. */
export async function archiveTask(id: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/tasks/${encodeURIComponent(id)}/archive`, { method: "POST" });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Fetch full task snapshot by id. */
export async function getTask(id: string): Promise<TaskSnapshot> {
  return fetchJson<TaskSnapshot>(`/tasks/${encodeURIComponent(id)}`);
}

/** Fetch raw content of a task output file. */
export async function getTaskOutput(id: string, filename: string): Promise<string> {
  const res = await fetch(`${BASE_URL}/tasks/${encodeURIComponent(id)}/output/${encodeURIComponent(filename)}`);
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
  return res.text();
}

// ---------------------------------------------------------------------------
// Agent-scoped tasklist API functions
// ---------------------------------------------------------------------------

/** List active + recent tasklists for an agent. */
export async function listAgentTasklists(
  agentId: string,
): Promise<ListTasklistsResponse> {
  return fetchJson<ListTasklistsResponse>(
    `/agents/${encodeURIComponent(agentId)}/tasklists`,
  );
}

/** Get a single agent-owned tasklist by id. */
export async function getAgentTasklist(
  agentId: string,
  tasklistId: string,
): Promise<Tasklist> {
  return fetchJson<Tasklist>(
    `/agents/${encodeURIComponent(agentId)}/tasklists/${encodeURIComponent(tasklistId)}`,
  );
}

/** Append a task to an agent-owned tasklist. */
export async function appendAgentTask(
  agentId: string,
  tasklistId: string,
  req: AppendTaskRequest,
): Promise<Tasklist> {
  return fetchJson<Tasklist>(
    `/agents/${encodeURIComponent(agentId)}/tasklists/${encodeURIComponent(tasklistId)}/tasks`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(req),
    },
  );
}

/** Create an agent-owned tasklist. The Todo panel uses this with empty
 *  `groups` + `allow_empty_groups: true` to draft a Paused list the user then
 *  populates via the composer and commits with {@link startAgentTasklist}. */
export async function createAgentTasklist(
  agentId: string,
  req: CreateTasklistRequest,
): Promise<Tasklist> {
  return fetchJson<Tasklist>(
    `/agents/${encodeURIComponent(agentId)}/tasklists`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(req),
    },
  );
}

/** Start (commit) a drafted agent-owned tasklist: flips Paused→Active so the
 *  feeder begins classifying + dispatching its staged tasks. */
export async function startAgentTasklist(
  agentId: string,
  tasklistId: string,
): Promise<Tasklist> {
  return fetchJson<Tasklist>(
    `/agents/${encodeURIComponent(agentId)}/tasklists/${encodeURIComponent(tasklistId)}/status`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ status: "active" }),
    },
  );
}

/** Stop an agent-owned tasklist (sets status to "stopped"/Cancelled). */
export async function stopAgentTasklist(
  agentId: string,
  tasklistId: string,
): Promise<Tasklist> {
  return fetchJson<Tasklist>(
    `/agents/${encodeURIComponent(agentId)}/tasklists/${encodeURIComponent(tasklistId)}/status`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ status: "stopped" }),
    },
  );
}

/** Skip a Pending task in an agent-owned tasklist. */
export async function skipAgentTask(
  agentId: string,
  tasklistId: string,
  taskId: string,
): Promise<Tasklist> {
  return fetchJson<Tasklist>(
    `/agents/${encodeURIComponent(agentId)}/tasklists/${encodeURIComponent(tasklistId)}/tasks/${encodeURIComponent(taskId)}/skip`,
    { method: "POST" },
  );
}

/** Manually complete a phase in a task. */
export async function completePhase(taskId: string, phase: string): Promise<TaskSnapshot> {
  return fetchJson<TaskSnapshot>(`/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phase)}/complete`, {
    method: "POST",
  });
}

/** Start a pending task. */
export async function startTask(taskId: string): Promise<TaskSnapshot> {
  return fetchJson<TaskSnapshot>(`/tasks/${encodeURIComponent(taskId)}/start`, {
    method: "POST",
  });
}

/** Resume a paused task. */
export async function resumeTask(taskId: string): Promise<TaskSnapshot> {
  return fetchJson<TaskSnapshot>(`/tasks/${encodeURIComponent(taskId)}/resume`, {
    method: "POST",
  });
}

/** Cancel a running task. */
export async function cancelTask(taskId: string): Promise<TaskSnapshot> {
  return fetchJson<TaskSnapshot>(`/tasks/${encodeURIComponent(taskId)}/cancel`, {
    method: "POST",
  });
}

// ---------------------------------------------------------------------------
// Phase chat API functions
// ---------------------------------------------------------------------------

/** Fetch all messages for a task phase. */
export async function getPhaseMessages(taskId: string, phaseId: string): Promise<TranscriptEntry[]> {
  return fetchJson<TranscriptEntry[]>(
    `/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phaseId)}/messages`,
  );
}

/** Send a user message to a phase agent. */
export async function sendPhaseMessage(
  taskId: string,
  phaseId: string,
  content: string,
  attachmentIds?: string[],
): Promise<{ message_id: string; status: string }> {
  const payload: Record<string, unknown> = { content };
  if (attachmentIds && attachmentIds.length > 0) {
    payload.attachment_ids = attachmentIds;
  }
  return fetchJson(`/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phaseId)}/messages`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
}

// ---------------------------------------------------------------------------
// Phase Attachment API functions
// ---------------------------------------------------------------------------

/** Upload a file attachment to a phase. */
export async function uploadPhaseAttachment(taskId: string, phaseId: string, file: File): Promise<Attachment> {
  const formData = new FormData();
  formData.append("file", file);
  const res = await fetch(
    `${BASE_URL}/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phaseId)}/attachments`,
    { method: "POST", body: formData },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
  return res.json() as Promise<Attachment>;
}

/** Add a folder reference attachment to a phase. */
export async function addPhaseFolderReference(taskId: string, phaseId: string, path: string): Promise<Attachment> {
  return fetchJson<Attachment>(
    `/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phaseId)}/attachments/folder`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path }),
    },
  );
}

/** Delete a phase attachment. */
export async function deletePhaseAttachment(taskId: string, phaseId: string, attachmentId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phaseId)}/attachments/${encodeURIComponent(attachmentId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Get the download URL for a phase attachment. */
export function getPhaseAttachmentUrl(taskId: string, phaseId: string, attachmentId: string): string {
  return `${BASE_URL}/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phaseId)}/attachments/${encodeURIComponent(attachmentId)}`;
}

/** Get metadata for a phase attachment. */
export async function getPhaseAttachmentInfo(taskId: string, phaseId: string, attachmentId: string): Promise<Attachment> {
  return fetchJson<Attachment>(
    `/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phaseId)}/attachments/${encodeURIComponent(attachmentId)}/info`,
  );
}

// ---------------------------------------------------------------------------
// Search API functions
// ---------------------------------------------------------------------------

export interface SearchResultItem {
  agent_id: string;
  agent_name: string;
  entry: TranscriptEntry;
}

export interface SearchResponse {
  results: SearchResultItem[];
}

/** Fetch messages around a target timestamp for an agent (for navigating to a specific message). */
export async function getMessagesAround(
  agentId: string,
  targetTs: string,
  windowSize = 25,
): Promise<PaginatedMessagesResponse> {
  // Fetch `windowSize` messages before and after the target timestamp
  // The backend supports before/after ISO timestamp filters
  const params = new URLSearchParams({
    before: new Date(new Date(targetTs).getTime() + 1).toISOString(), // inclusive of target
    last: String(windowSize * 2),
  });
  return fetchJson<PaginatedMessagesResponse>(`/agents/${encodeURIComponent(agentId)}/messages?${params}`);
}

/** Search messages across agents. */
export async function searchMessages(
  query: string,
  limit?: number,
  agentId?: string,
): Promise<SearchResponse> {
  const params = new URLSearchParams({ q: query });
  if (limit != null) params.set("limit", String(limit));
  if (agentId) params.set("agent_id", agentId);
  // Ripgrep sweep across the full transcript history — scales with how many
  // messages the user has accumulated.
  return fetchJson<SearchResponse>(`/search?${params}`, {}, BULK_FETCH_TIMEOUT_MS);
}

/** Cold-start the agent for a phase. */
export async function startPhaseAgent(
  taskId: string,
  phaseId: string,
): Promise<{ message_id: string; status: string }> {
  return fetchJson(`/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phaseId)}/start`, {
    method: "POST",
  });
}

/** Submit input form values for an input phase. */
export async function submitPhaseInput(
  taskId: string,
  phaseId: string,
  values: Record<string, string>,
): Promise<{ status: string; output: string }> {
  return fetchJson(`/tasks/${encodeURIComponent(taskId)}/phases/${encodeURIComponent(phaseId)}/submit-input`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(values),
  });
}

/** Build the SSE stream URL for a task (all phases). */
export function taskStreamUrl(taskId: string): string {
  return `${BASE_URL}/tasks/${encodeURIComponent(taskId)}/stream`;
}

// ---------------------------------------------------------------------------
// Scheduled Task data shape
//
// The dedicated ScheduledTask CRUD endpoints/UI are gone — Assignments (see
// the Assignment section below) replaced that surface. This shape survives
// because the Assignments calendar (ScheduledCalendar) and its occurrence-
// expansion/timezone logic (lib/scheduleOccurrences.ts) still consume it:
// assignmentToScheduledTaskWithOwner (components/assignments/
// assignmentAdapters.ts) projects an `Assignment` onto this shape so that
// machinery is reused verbatim rather than re-implemented.
// ---------------------------------------------------------------------------

/** Which thread a scheduled task's fires land in. `main` (the default, and
 *  what every task effectively did before this field existed) posts into the
 *  agent's default thread — the same one interactive chat uses. `fresh`
 *  creates a new disposable thread every fire. `dedicated` reuses one thread
 *  across every fire, created lazily on first fire. Ignored for team-shared
 *  tasks (`is_team: true`) — team dispatch has no per-thread routing. */
export type ScheduledTaskThreadPolicy = "fresh" | "main" | "dedicated";

export interface ScheduledTask {
  id: string;
  agent_id: string;
  /** Optional human-readable label shown in the Scheduled UI instead of a
   *  truncated prompt excerpt. `null` for tasks that were never named. */
  name: string | null;
  is_team: boolean;
  cron: string | null;
  prompt: string;
  working_directory: string | null;
  is_recurring: boolean;
  created_at: string;
  last_run_at: string | null;
  next_fire_at: string | null;
  enabled: boolean;
  expires_at: string | null;
  thread_policy: ScheduledTaskThreadPolicy;
  /** Set once a `dedicated`-policy task has fired at least once; the same
   *  thread id is reused on every subsequent fire. Server-managed — never
   *  client-settable. */
  dedicated_thread_id?: string | null;
}

// ---------------------------------------------------------------------------
// Agent Skills API functions
// ---------------------------------------------------------------------------

/** Shape of a single skill entry returned by the backend `SkillDto`. */
export interface Skill {
  id: string;
  title: string;
  description: string;
  added_by: "user" | "agent" | "github" | "link";
  source_url?: string | null;
  auto_sync: boolean;
  enabled: boolean;
  updated_on: string;
  added_on: string;
  usage_count?: number;
  last_used?: string | null;
  /** Distinguishes user-pool skills from plugin-pool skills in the combined list. */
  source?: "user" | "plugin";
}

/** List all skills present in an agent's skills directory. */
export async function listAgentSkills(agentId: string): Promise<Skill[]> {
  return fetchJson<Skill[]>(`/agents/${encodeURIComponent(agentId)}/skills`);
}

/** Re-scan skills dir + `git pull` every auto-sync github skill. */
export async function refreshAgentSkills(agentId: string): Promise<Skill[]> {
  return fetchJson<Skill[]>(`/agents/${encodeURIComponent(agentId)}/skills/refresh`, {
    method: "POST",
  });
}

/** Copy a local folder into the agent's skills directory. Returns every
 *  skill discovered by recursively walking the imported bundle for `SKILL.md`
 *  files (or a single fallback entry when the bundle has none). */
export async function importAgentSkillFolder(agentId: string, srcPath: string): Promise<Skill[]> {
  // Recursive copy of a user-chosen skill folder of unbounded size into the
  // agent's skills directory.
  return fetchJson<Skill[]>(`/agents/${encodeURIComponent(agentId)}/skills/import-folder`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ src_path: srcPath }),
  }, BULK_FETCH_TIMEOUT_MS);
}

/** Copy a single `.md` file into the agent's skills directory as a flat skill. */
export async function importAgentSkillFile(agentId: string, srcPath: string): Promise<Skill> {
  return fetchJson<Skill>(`/agents/${encodeURIComponent(agentId)}/skills/import-file`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ src_path: srcPath }),
  });
}

/** Remove a skill folder (or flat `.md`) and its sidecar manifest. */
export async function deleteAgentSkill(agentId: string, skillId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/skills/${encodeURIComponent(skillId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Update `enabled` and/or `auto_sync` on a skill's sidecar manifest. */
export async function patchAgentSkill(
  agentId: string,
  skillId: string,
  patch: { enabled?: boolean; autoSync?: boolean },
): Promise<Skill> {
  const body: Record<string, unknown> = {};
  if (patch.enabled !== undefined) body.enabled = patch.enabled;
  if (patch.autoSync !== undefined) body.auto_sync = patch.autoSync;
  return fetchJson<Skill>(
    `/agents/${encodeURIComponent(agentId)}/skills/${encodeURIComponent(skillId)}`,
    {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    },
  );
}

// ---------------------------------------------------------------------------
// Launchpad convention-folder skills API functions
//
// A separate pool source from the user/plugin pools above: human-dropped
// skill folders under `<data_root>/.launchpad/skills` (global) and
// `<focus_path>/.launchpad/skills` (project). See
// convention-folder skills.
// ---------------------------------------------------------------------------

/** One convention-folder skill discovered under a `.launchpad/skills` directory. */
export interface LaunchpadSkillEntry {
  name: string;
  description?: string | null;
  path: string;
}

/** List convention-folder skills from the shared global root
 *  (`<data_root>/.launchpad/skills`). Not agent-scoped. */
export async function listLaunchpadGlobalSkills(): Promise<LaunchpadSkillEntry[]> {
  const res = await fetchJson<{ skills: LaunchpadSkillEntry[] }>("/skills/launchpad/global");
  return res.skills;
}

/** List convention-folder skills scoped to a project (`<focusPath>/.launchpad/skills`),
 *  plus the canonicalized `project_key` to echo back in enable/promote calls. */
export async function listLaunchpadProjectSkills(
  focusPath: string,
): Promise<{ project_key: string; skills: LaunchpadSkillEntry[] }> {
  const params = new URLSearchParams({ focus_path: focusPath });
  return fetchJson<{ project_key: string; skills: LaunchpadSkillEntry[] }>(
    `/skills/launchpad/project?${params}`,
  );
}

/** Enable/disable a global convention-folder skill for one agent. */
export async function setLaunchpadGlobalSkillEnabled(
  agentId: string,
  skillName: string,
  enabled: boolean,
): Promise<{ skill_name: string; enabled: boolean }> {
  return fetchJson(`/agents/${encodeURIComponent(agentId)}/launchpad-skills/global`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ skill_name: skillName, enabled }),
  });
}

/** Enable/disable a project convention-folder skill for one agent, scoped to `projectKey`. */
export async function setLaunchpadProjectSkillEnabled(
  agentId: string,
  projectKey: string,
  skillName: string,
  enabled: boolean,
): Promise<{ project_key: string; skill_name: string; enabled: boolean }> {
  return fetchJson(`/agents/${encodeURIComponent(agentId)}/launchpad-skills/project`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ project_key: projectKey, skill_name: skillName, enabled }),
  });
}

/** Copy a project convention-folder skill up into the shared global root
 *  ("Make available globally"). Throws `ApiError` with `status === 409` when
 *  a global skill with that name already exists — the caller should surface
 *  this as a non-fatal "already exists globally" notice, not an error banner. */
export async function promoteLaunchpadSkill(
  focusPath: string,
  skillName: string,
): Promise<{ promoted: string }> {
  const res = await fetch(`${BASE_URL}/skills/launchpad/promote`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ focus_path: focusPath, skill_name: skillName }),
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let message = body || `Request failed with status ${res.status}`;
    try {
      const parsed = JSON.parse(body);
      if (parsed && typeof parsed.error === "string") message = parsed.error;
    } catch {
      // body wasn't JSON — fall through with raw text
    }
    throw new ApiError(res.status, message);
  }
  return res.json();
}

// ---------------------------------------------------------------------------
// Agent Rules API functions
// ---------------------------------------------------------------------------

/** Source that introduced a rule into the agent's library — shared with skills. */
export type AddedBy = Skill["added_by"];

/** Shape of a single rule entry returned by the backend `RuleDto`. Unlike
 *  skills, the `content` field carries the full markdown body so the viewer
 *  modal does not need a second round-trip. */
export interface Rule {
  id: string;
  title: string;
  description: string;
  added_by: AddedBy;
  source_url?: string | null;
  auto_sync: boolean;
  enabled: boolean;
  updated_on: string;
  added_on: string;
  content: string;
}

/** List every rule discovered under the agent's rules directory (recursive). */
export async function listRules(agentId: string): Promise<Rule[]> {
  return fetchJson<Rule[]>(`/agents/${encodeURIComponent(agentId)}/rules`);
}

/** Import a single `.md` file as a rule bundle. Returns every rule entry
 *  discovered in the newly imported bundle. */
export async function importAgentRuleFile(agentId: string, srcPath: string): Promise<Rule[]> {
  return fetchJson<Rule[]>(`/agents/${encodeURIComponent(agentId)}/rules/import-file`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ src_path: srcPath }),
  });
}

/** Import a local folder as a rule bundle. Returns every rule entry
 *  discovered by recursively walking the imported bundle. */
export async function importAgentRuleFolder(agentId: string, srcPath: string): Promise<Rule[]> {
  // Recursive copy of a user-chosen rules folder of unbounded size into the
  // agent's rules directory.
  return fetchJson<Rule[]>(`/agents/${encodeURIComponent(agentId)}/rules/import-folder`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ src_path: srcPath }),
  }, BULK_FETCH_TIMEOUT_MS);
}

/** Download a single `.md` via HTTP GET and write it to the rules directory. */
export async function importAgentRuleLink(agentId: string, url: string): Promise<Rule[]> {
  // Server-side HTTP GET of an arbitrary user-supplied URL, then a write to
  // the rules directory — a single remote hop we don't control.
  return fetchJson<Rule[]>(`/agents/${encodeURIComponent(agentId)}/rules/import-link`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ url }),
  }, EXTERNAL_WORK_TIMEOUT_MS);
}

/** Update `enabled` and/or `auto_sync` on a rule's sidecar manifest. */
export async function patchAgentRule(
  agentId: string,
  ruleId: string,
  patch: { enabled?: boolean; autoSync?: boolean },
): Promise<Rule> {
  const body: Record<string, unknown> = {};
  if (patch.enabled !== undefined) body.enabled = patch.enabled;
  if (patch.autoSync !== undefined) body.auto_sync = patch.autoSync;
  return fetchJson<Rule>(
    `/agents/${encodeURIComponent(agentId)}/rules/${encodeRuleId(ruleId)}`,
    {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    },
  );
}

/** Delete a top-level rule bundle (cascade). Nested ids are rejected by the
 *  backend — callers must disable the nested rule or remove the bundle. */
export async function deleteAgentRule(agentId: string, ruleId: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/agents/${encodeURIComponent(agentId)}/rules/${encodeRuleId(ruleId)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Re-scan the rules dir and `git pull` every auto-sync github bundle. */
export async function refreshAgentRules(agentId: string): Promise<Rule[]> {
  // Re-fetches every linked rule for the agent — one upstream HTTP hop per
  // link, run sequentially, so the total scales with the link count.
  return fetchJson<Rule[]>(`/agents/${encodeURIComponent(agentId)}/rules/refresh`, {
    method: "POST",
  }, BULK_FETCH_TIMEOUT_MS);
}

// Rule ids may contain `/` segment separators (e.g. `bundle/inner/strict.md`).
// The backend route captures the id via the `{*rule_id}` wildcard, so we keep
// the slashes literal and only percent-encode the individual segments.
function encodeRuleId(ruleId: string): string {
  return ruleId.split("/").map(encodeURIComponent).join("/");
}

// ---------------------------------------------------------------------------
// Agent Instructions API functions
// ---------------------------------------------------------------------------

/** Shape of a single instruction file returned by the backend `InstructionDto`. */
export interface Instruction {
  id: string;
  name: string;
  path: string;
  enabled: boolean;
  updated_on: string;
  content: string;
}

/** List instruction files at the root of the agent home that match the user's
 *  configured `instruction_filenames` (case-insensitive). */
export async function listInstructions(agentId: string): Promise<Instruction[]> {
  return fetchJson<Instruction[]>(`/agents/${encodeURIComponent(agentId)}/instructions`);
}

/** Toggle the per-file `enabled` state for a single instruction. */
export async function patchInstruction(
  agentId: string,
  id: string,
  enabled: boolean,
): Promise<Instruction> {
  return fetchJson<Instruction>(
    `/agents/${encodeURIComponent(agentId)}/instructions/${encodeURIComponent(id)}`,
    {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ enabled }),
    },
  );
}

/** Fetch the global `instruction_filenames` preference list. */
export async function getInstructionFilenames(): Promise<string[]> {
  return fetchJson<string[]>("/preferences/instruction-filenames");
}

/** Replace the global `instruction_filenames` list. The backend returns the
 *  normalized list (trimmed + case-insensitively deduped). */
export async function setInstructionFilenames(filenames: string[]): Promise<string[]> {
  return fetchJson<string[]>("/preferences/instruction-filenames", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(filenames),
  });
}

// ---------------------------------------------------------------------------
// Project API functions
// ---------------------------------------------------------------------------

export interface CreateProjectRequest {
  goal: string;
  agent_id: string;
  name?: string;
  emoji?: string;
  working_dir?: string;
  attachments?: string[];
}

export interface PatchProjectRequest {
  name?: string;
  emoji?: string | null;
  spec?: string | null;
  status?: import("../types/api").ProjectStatus;
  working_dir?: string | null;
}

/** Create a new project. */
export async function createProject(req: CreateProjectRequest): Promise<Project> {
  return fetchJson<Project>("/projects", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
}

/** List all projects as lightweight snapshots. */
export async function getProjects(): Promise<ProjectListItem[]> {
  return fetchJson<ProjectListItem[]>("/projects");
}

/** Fetch full project details by id. */
export async function getProject(id: string): Promise<Project> {
  return fetchJson<Project>(`/projects/${encodeURIComponent(id)}`);
}

/** Partially update a project. */
export async function updateProject(id: string, patch: PatchProjectRequest): Promise<Project> {
  return fetchJson<Project>(`/projects/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });
}

/** Delete a project. */
export async function deleteProject(id: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/projects/${encodeURIComponent(id)}`, { method: "DELETE" });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Send a user message to a project's agent. */
export async function sendProjectMessage(
  projectId: string,
  content: string,
  attachmentIds?: string[],
): Promise<MessageAck> {
  const payload: Record<string, unknown> = { content };
  if (attachmentIds && attachmentIds.length > 0) {
    payload.attachment_ids = attachmentIds;
  }
  return fetchJson<MessageAck>(`/projects/${encodeURIComponent(projectId)}/messages`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
}

/** Fetch paginated project conversation history. `signal` is forwarded to
 *  `fetchJson` — see `getAgent`'s doc comment. */
export async function getProjectMessages(
  projectId: string,
  cursor?: PaginationCursor,
  last = 50,
  signal?: AbortSignal,
): Promise<ProjectMessagesResponse> {
  if (cursor) {
    const params = new URLSearchParams({
      cursor_offset: String(cursor.byte_offset),
      cursor_message_id: cursor.last_message_id,
      cursor_timestamp: cursor.timestamp,
      last: String(last),
    });
    return fetchJson<ProjectMessagesResponse>(
      `/projects/${encodeURIComponent(projectId)}/messages?${params}`,
      { signal },
    );
  }
  // No cursor (first page / refetch-from-latest): still pass `last` explicitly.
  // Without this the request carries no query string at all and the server
  // falls back to its own default of 50, silently reclamping any previously
  // expanded window (e.g. after "load more") back down on every refetch.
  const params = new URLSearchParams({ last: String(last) });
  return fetchJson<ProjectMessagesResponse>(
    `/projects/${encodeURIComponent(projectId)}/messages?${params}`,
    { signal },
  );
}

/** Build the SSE stream URL for a project. */
export function projectStreamUrl(projectId: string): string {
  return `${BASE_URL}/projects/${encodeURIComponent(projectId)}/stream`;
}

/** Cancel an in-flight project run. */
export async function cancelProjectRun(projectId: string): Promise<void> {
  await fetch(`${BASE_URL}/projects/${encodeURIComponent(projectId)}/cancel`, {
    method: "POST",
  });
}

/** Upload a file attachment scoped to the project channel. */
export async function uploadProjectAttachment(
  projectId: string,
  file: File,
): Promise<Attachment> {
  const form = new FormData();
  form.append("file", file);
  const res = await fetch(
    `${BASE_URL}/projects/${encodeURIComponent(projectId)}/attachments`,
    { method: "POST", body: form },
  );
  if (!res.ok) throw new Error(`Upload failed: ${res.status}`);
  return res.json() as Promise<Attachment>;
}

/** Upload a folder reference scoped to the project channel. */
export async function uploadProjectFolderReference(
  projectId: string,
  path: string,
): Promise<Attachment> {
  return fetchJson<Attachment>(
    `/projects/${encodeURIComponent(projectId)}/attachments/folder`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path }),
    },
  );
}

/** Delete a project-channel attachment by id. */
export async function deleteProjectAttachment(
  projectId: string,
  attachmentId: string,
): Promise<void> {
  await fetch(
    `${BASE_URL}/projects/${encodeURIComponent(projectId)}/attachments/${encodeURIComponent(attachmentId)}`,
    { method: "DELETE" },
  );
}

/** Resolved URL for serving a project-channel attachment (image preview etc.). */
export function getProjectAttachmentUrl(
  projectId: string,
  attachmentId: string,
): string {
  return `${BASE_URL}/projects/${encodeURIComponent(projectId)}/attachments/${encodeURIComponent(attachmentId)}`;
}

/** List active + recent tasklists tagged to a project. */
export async function listProjectTasklists(projectId: string): Promise<ListTasklistsResponse> {
  return fetchJson<ListTasklistsResponse>(
    `/projects/${encodeURIComponent(projectId)}/tasklists`,
  );
}

// ---------------------------------------------------------------------------
// Thread API functions
// ---------------------------------------------------------------------------

/** List all threads for an agent. The server lazily ensures the default thread
 *  exists. `signal` is forwarded to `fetchJson` (composed with its own
 *  timeout, not clobbering it) — used by HomeSidebar's eager thread
 *  hydration fan-out so unmounting/re-prioritizing can actually cancel a
 *  still-outstanding request instead of merely ignoring its result. */
export async function listThreads(agentId: string, signal?: AbortSignal): Promise<Thread[]> {
  return fetchJson<Thread[]>(`/agents/${encodeURIComponent(agentId)}/threads`, { signal });
}

/** List every thread across every agent in one call, grouped by owning agent
 *  id: `{ "<agent_id>": [Thread, ...], ... }`. Metadata only, server-side
 *  grouped (see `list_all_threads` in `routes/threads.rs`) — agents with zero
 *  threads are simply absent from the response, not present with an empty
 *  array. Used for bulk hydration; callers seeding a map from this must add
 *  their own empty-array fallback for known agents missing from the result. */
export async function listAllThreads(signal?: AbortSignal): Promise<Record<string, Thread[]>> {
  return fetchJson<Record<string, Thread[]>>(`/threads`, { signal });
}

export interface CreateThreadRequest {
  title?: string | null;
  kind: "fresh" | "branch";
  branch_source?: BranchSource;
}

/** Create a fresh or branch thread for an agent. Omit `branch_source` for fresh threads. */
export async function createThread(agentId: string, req: CreateThreadRequest): Promise<Thread> {
  return fetchJson<Thread>(`/agents/${encodeURIComponent(agentId)}/threads`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
}

/** Fetch a single thread by id. Throws on 404. */
export async function getThread(threadId: string): Promise<Thread> {
  return fetchJson<Thread>(`/threads/${encodeURIComponent(threadId)}`);
}

/** Rename a thread. Pass `null` to clear the title. */
export async function renameThread(threadId: string, title: string | null): Promise<Thread> {
  return fetchJson<Thread>(`/threads/${encodeURIComponent(threadId)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ title }),
  });
}

/** Delete a thread by id. Default threads cannot be deleted (server returns 400). */
export async function deleteThread(threadId: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/threads/${encodeURIComponent(threadId)}`, { method: "DELETE" });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Archive a thread — hides it from the tab strip, overflow panel, Threads
 *  panel's main list, and Home's thread list without deleting anything.
 *  Default threads cannot be archived (server returns 400). */
export async function archiveThread(threadId: string): Promise<Thread> {
  return fetchJson<Thread>(`/threads/${encodeURIComponent(threadId)}/archive`, { method: "POST" });
}

/** Reverse of `archiveThread` — restores the thread to every surface it was
 *  hidden from. */
export async function unarchiveThread(threadId: string): Promise<Thread> {
  return fetchJson<Thread>(`/threads/${encodeURIComponent(threadId)}/unarchive`, { method: "POST" });
}

// ---------------------------------------------------------------------------
// Assignment API functions
// ---------------------------------------------------------------------------

export interface CreateAssignmentRequest {
  name: string;
  instruction: string;
  /** Focus path passed through as the fired run's `focus_path`. */
  working_directory?: string | null;
  trigger: AssignmentTrigger;
  bindings?: AssignmentBinding[];
  output_mode?: OutputMode;
  /** Where this assignment's runs land. When omitted, the server resolves a
   *  trigger-dependent default: `Cron` -> "main", `Webhook` -> "fresh".
   *  There's no `dedicated_thread_id` here: it's server-managed, claimed
   *  automatically on first fire. */
  thread_policy?: AssignmentThreadPolicy;
  enabled?: boolean;
  /** Optional cutoff after which the assignment stops firing on its own. */
  expires_at?: string | null;
}

export interface PatchAssignmentRequest {
  name?: string;
  instruction?: string;
  working_directory?: string | null;
  /** Full trigger replacement when present; absent fields are left unchanged. */
  trigger?: AssignmentTrigger;
  bindings?: AssignmentBinding[];
  output_mode?: OutputMode;
  thread_policy?: AssignmentThreadPolicy;
  enabled?: boolean;
  expires_at?: string | null;
}

export interface TriggerAssignmentRequest {
  token?: string | null;
  payload_summary?: string | null;
}

/** List all assignments owned by an agent. */
export async function listAssignments(agentId: string): Promise<Assignment[]> {
  return fetchJson<Assignment[]>(`/agents/${encodeURIComponent(agentId)}/assignments`);
}

/** Create a new assignment for an agent. */
export async function createAssignment(agentId: string, req: CreateAssignmentRequest): Promise<Assignment> {
  return fetchJson<Assignment>(`/agents/${encodeURIComponent(agentId)}/assignments`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
}

/** Fetch a single assignment by id. Throws on 404. */
export async function getAssignment(assignmentId: string): Promise<Assignment> {
  return fetchJson<Assignment>(`/assignments/${encodeURIComponent(assignmentId)}`);
}

/** Update mutable fields on an assignment. Absent fields are left unchanged. */
export async function updateAssignment(assignmentId: string, patch: PatchAssignmentRequest): Promise<Assignment> {
  return fetchJson<Assignment>(`/assignments/${encodeURIComponent(assignmentId)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });
}

/** Delete an assignment by id. Run history is preserved server-side. */
export async function deleteAssignment(assignmentId: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/assignments/${encodeURIComponent(assignmentId)}`, { method: "DELETE" });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** List run history for an assignment. The server returns rows newest-first. */
export async function listAssignmentRuns(assignmentId: string): Promise<AssignmentRun[]> {
  return fetchJson<AssignmentRun[]>(`/assignments/${encodeURIComponent(assignmentId)}/runs`);
}

/** Fire an assignment immediately. Returns the queued run row (server responds 202). */
export async function triggerAssignment(assignmentId: string, body?: TriggerAssignmentRequest): Promise<AssignmentRun> {
  return fetchJson<AssignmentRun>(`/assignments/${encodeURIComponent(assignmentId)}/trigger`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body ?? {}),
  });
}

// ---------------------------------------------------------------------------
// Webhook route API functions (push-side `Webhook` trigger support)
// ---------------------------------------------------------------------------

/** Whether a secret currently resolves for a webhook route
 *  (`GET /webhooks/{route_name}/secret`) — never the secret value itself,
 *  since the store is write-only. */
export interface WebhookRouteSecretStatus {
  configured: boolean;
}

/** Fetch whether `routeName` currently has a resolvable HMAC secret. Safe to
 *  call for a route that doesn't exist yet as an assignment — resolves
 *  `{ configured: false }` rather than throwing. */
export async function getWebhookRouteSecretStatus(routeName: string): Promise<WebhookRouteSecretStatus> {
  return fetchJson<WebhookRouteSecretStatus>(`/webhooks/${encodeURIComponent(routeName)}/secret`);
}

/** Set (or replace/rotate) the HMAC signing secret for a webhook route
 *  (`PUT /webhooks/{route_name}/secret`). Write-only: the secret is never
 *  returned by this or any other endpoint — reveal/copy only works for a
 *  value the caller already holds client-side (freshly generated or typed).
 *  Throws `ApiError` (400) on an empty secret. */
export async function setWebhookRouteSecret(routeName: string, secret: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/webhooks/${encodeURIComponent(routeName)}/secret`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ secret }),
  });
  if (!res.ok) return throwApiError(res);
}

/** Request body for `POST /webhook-test` — a stateless dry run of a route's
 *  `events`/`filters`/`prompt_template`/`deliver` config against a sample
 *  payload, evaluated with the exact same functions the real gateway uses.
 *  Takes the draft config directly (not an already-saved route) so it works
 *  before the assignment has ever been saved. No agent is ever spawned and
 *  no `github_comment` is ever posted. */
export interface TestWebhookRouteRequest {
  events: string[];
  filters: WebhookFilter | null;
  prompt_template: string | null;
  deliver: WebhookDeliverTarget;
  /** Simulated `X-GitHub-Event`/`X-Event-Type` header value. */
  event_type: string | null;
  /** Sample inbound JSON body to evaluate the config against. */
  payload: unknown;
}

/** Result of a `POST /webhook-test` dry run. */
export interface TestWebhookRouteResponse {
  /** True if `events`/`filters` would let this sample payload through. */
  matched: boolean;
  /** The `deliver` target a matching payload would be routed to. */
  deliver: WebhookDeliverTarget;
  /** True only when `matched` and `deliver` is `agent` — this sample would
   *  actually start an agent run were this a real request. */
  would_start_agent: boolean;
  /** The rendered `prompt_template` — exactly what the agent's instruction
   *  (or the `deliver_only`/`github_comment` payload) would contain. `null`
   *  when no template is set, or the sample was filtered out. */
  rendered_instruction: string | null;
}

/** Dry-run a webhook route's config against a sample payload
 *  (`POST /webhook-test`) — backs the editor's "Send test webhook" button. */
export async function testWebhookRoute(req: TestWebhookRouteRequest): Promise<TestWebhookRouteResponse> {
  return fetchJson<TestWebhookRouteResponse>("/webhook-test", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
}

/** Which trigger kind the instruction being refined belongs to — lets the
 *  backend tailor its rewrite (e.g. webhook/poll_connector instructions may
 *  reference `{dot.path}`/`{__raw__}` payload placeholders that a cron
 *  instruction never has available). Optional on the wire; omitting it
 *  falls back to the backend's own default. */
export type RefineTemplateMode = "cron" | "webhook" | "poll_connector";

/** Request body for `POST /prompt-refine` — asks the given agent's own
 *  provider/model to rewrite an assignment's Instruction text into a
 *  clearer instruction, preserving every `{dot.path}`/`{__raw__}`
 *  placeholder token verbatim. */
export interface RefineTemplateRequest {
  agent_id: string;
  prompt_template: string;
  mode?: RefineTemplateMode;
}

/** Result of a `POST /prompt-refine` call. */
export interface RefineTemplateResponse {
  refined_template: string;
}

/** Ask an agent's provider to rewrite the assignment editor's Instruction
 *  text (`POST /prompt-refine`) — backs the editor's "Refine" button,
 *  available for every trigger mode. */
export async function refineTemplate(req: RefineTemplateRequest): Promise<RefineTemplateResponse> {
  // Blocks on a full LLM provider round-trip that rewrites the prompt
  // template server-side.
  return fetchJson<RefineTemplateResponse>("/prompt-refine", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  }, EXTERNAL_WORK_TIMEOUT_MS);
}

// ---------------------------------------------------------------------------
// MCP Servers API functions
// ---------------------------------------------------------------------------

export type { McpServerStatus, AddMcpServerRequest } from "../types/connectors";
import type { McpServerStatus, AddMcpServerRequest } from "../types/connectors";

/** List all configured MCP servers and their current connection state. */
export async function listMcpServers(): Promise<McpServerStatus[]> {
  return fetchJson<McpServerStatus[]>("/mcp-servers");
}

/** Add and connect a new MCP server. */
export async function addMcpServer(req: AddMcpServerRequest): Promise<McpServerStatus> {
  // Spawns the MCP server subprocess and completes the initialize/list-tools
  // handshake before responding — third-party startup time we don't control.
  return fetchJson<McpServerStatus>("/mcp-servers", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  }, EXTERNAL_WORK_TIMEOUT_MS);
}

/** Remove an MCP server from config and shut down its connection. */
export async function deleteMcpServer(name: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/mcp-servers/${encodeURIComponent(name)}`, { method: "DELETE" });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Start the OAuth authorization flow for a server in needs_auth state.
 *  Returns 202 with the authorization URL to open in a browser. */
export async function authorizeMcpServer(name: string): Promise<{ auth_url: string }> {
  return fetchJson<{ auth_url: string }>(`/mcp-servers/${encodeURIComponent(name)}/authorize`, {
    method: "POST",
  });
}

// ---------------------------------------------------------------------------
// Providers API functions
// ---------------------------------------------------------------------------
//
// Global `providers.toml` credentials (one file per data root, not scoped to
// an agent). The read side (`getProviderStatuses`) never carries a plaintext
// key — only whether one is configured — so the frontend can show a status
// indicator without a secret ever entering browser memory. Writes go through
// a narrow per-provider PUT that merges into the existing file server-side.

export type { ProviderStatus } from "../types/api";
import type {
  AgentReasoningEffort,
  ProviderModelDiscoveryErrorCode,
  ProviderStatus,
  SetProviderRequest,
} from "../types/api";

/** Masked status for every known provider (anthropic/openai/openrouter/gemini). */
export async function getProviderStatuses(): Promise<ProviderStatus[]> {
  return fetchJson<ProviderStatus[]>("/providers");
}

/** Write (or overwrite) one provider's credentials in `providers.toml` —
 *  the API key plus the optional base URL / model / tuning-knob overrides
 *  that make self-hosted / OpenAI-compatible endpoints (Ollama, LM Studio,
 *  OpenRouter), non-default model IDs, and per-provider token/reasoning
 *  defaults work. Every `opts` field omitted or blank is left out of the
 *  request body entirely (rather than sent as `""`/`0`) so a save that only
 *  means to update the key doesn't clobber an already-persisted value — the
 *  server merges into the existing section. */
export async function setProviderApiKey(
  provider: string,
  apiKey: string,
  opts: {
    baseUrl?: string | null;
    model?: string | null;
    maxOutputTokens?: number | null;
    maxContextTokens?: number | null;
    reasoningEffort?: AgentReasoningEffort | null;
  } = {},
): Promise<void> {
  const body: SetProviderRequest = { api_key: apiKey };
  if (opts.baseUrl) body.base_url = opts.baseUrl;
  if (opts.model) body.model = opts.model;
  if (opts.maxOutputTokens) body.max_output_tokens = opts.maxOutputTokens;
  if (opts.maxContextTokens) body.max_context_tokens = opts.maxContextTokens;
  if (opts.reasoningEffort) body.reasoning_effort = opts.reasoningEffort;
  const res = await fetch(`${BASE_URL}/providers/${encodeURIComponent(provider)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${text}`);
  }
}

/** Clear one provider's stored API key. */
export async function deleteProviderApiKey(provider: string): Promise<void> {
  const res = await fetch(`${BASE_URL}/providers/${encodeURIComponent(provider)}`, { method: "DELETE" });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`API ${res.status}: ${body}`);
  }
}

/** Thrown by {@link getProviderModels} on a non-2xx response, and on a
 *  client-side timeout. `code` mirrors the JSON body's `code` field when the
 *  server attached one (`"auth_failure" | "network_failure" |
 *  "malformed_response"`) — absent for the plain 400/500 precondition
 *  failures (unknown/unconfigured provider, unreadable `providers.toml`). A
 *  client-side timeout carries `code: "network_failure"` too, matching how
 *  the server itself reports an upstream that never responds, so callers
 *  don't need a fourth case. */
export class ProviderModelDiscoveryError extends Error {
  code?: ProviderModelDiscoveryErrorCode;
  constructor(message: string, code?: ProviderModelDiscoveryErrorCode) {
    super(message);
    this.name = "ProviderModelDiscoveryError";
    this.code = code;
  }
}

/** Client-side deadline for {@link getProviderModels}. Matches — rather than
 *  undercuts — the ceiling the server itself already guarantees a response
 *  by: the discovery client's own connect+request timeouts bound the
 *  outbound provider call to 15s
 *  (`ao_engine_tools_provider_config::model_discovery::REQUEST_TIMEOUT`),
 *  and a 20s `TimeoutLayer` backstops the whole route server-side
 *  (`crates/ao-server/src/routes/mod.rs::PROVIDER_MODELS_ROUTE_TIMEOUT`).
 *  Matching that outer bound means the server's own structured
 *  `network_failure` response — with a real message — reaches this call
 *  before the abort below ever fires in the ordinary "upstream never
 *  responded" case; this exists purely so the "discovering" spinner can
 *  never hang past the server's own ceiling, even if the local connection
 *  itself stalls for a reason the server-side timeouts don't cover. */
const MODEL_DISCOVERY_TIMEOUT_MS = 20_000;

/** Live model IDs for a provider's stored key — `GET /providers/{name}/models`.
 *  This doubles as the app's only API-key validity check (there is
 *  deliberately no separate "test connection" endpoint): a 401/403 from
 *  upstream surfaces here as a {@link ProviderModelDiscoveryError} with
 *  `code: "auth_failure"`, which callers must treat as a soft, non-blocking
 *  warning rather than a reason to withhold saving — the key may already be
 *  persisted by the time this rejects. Bounded by {@link
 *  MODEL_DISCOVERY_TIMEOUT_MS} so a stalled upstream (including a
 *  user-supplied custom base URL) can never leave the caller's "discovering"
 *  state stuck forever. */
export async function getProviderModels(provider: string): Promise<string[]> {
  let res: Response;
  try {
    res = await fetch(`${BASE_URL}/providers/${encodeURIComponent(provider)}/models`, {
      signal: AbortSignal.timeout(MODEL_DISCOVERY_TIMEOUT_MS),
    });
  } catch (err) {
    if (err instanceof DOMException && err.name === "TimeoutError") {
      throw new ProviderModelDiscoveryError("Timed out waiting for the provider to respond.", "network_failure");
    }
    throw err;
  }
  if (!res.ok) {
    let parsed: { error?: string; code?: string } = {};
    try {
      parsed = await res.json();
    } catch {
      // Non-JSON body shouldn't happen per the route's contract; fall back
      // to a generic message below.
    }
    throw new ProviderModelDiscoveryError(
      parsed.error ?? `API ${res.status}`,
      parsed.code as ProviderModelDiscoveryErrorCode | undefined,
    );
  }
  return res.json() as Promise<string[]>;
}
