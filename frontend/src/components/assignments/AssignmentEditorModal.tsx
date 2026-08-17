import { useCallback, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { useNavigate } from "react-router-dom";
import { motion, AnimatePresence } from "framer-motion";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import { CalendarClock, FolderOpen, Loader2, Play, Sparkles, Trash2, X } from "lucide-react";
import { useAssignmentEditorModalStore } from "../../stores/assignmentEditorModalStore";
import { useChatStore } from "../../stores/chatStore";
import { getAppConfig, getAssignment, listMcpServers, refineTemplate } from "../../lib/api";
import type { CreateAssignmentRequest, PatchAssignmentRequest, RefineTemplateMode } from "../../lib/api";
import type {
  Assignment,
  AssignmentThreadPolicy,
  AssignmentTrigger,
  AssignmentWatchHealth,
  WatchContract,
} from "../../types/api";
import type { McpServerStatus } from "../../types/connectors";
import { CronPicker } from "../CronPicker";
import { DateTimePicker } from "../DateTimePicker";
import { switchToThread } from "../../lib/threadNavigation";
import { THREAD_POLICY_OPTIONS } from "./threadPolicyOptions";
import { AssignmentAgentTiles } from "./AssignmentAgentTiles";
import { WatchContractPanel } from "./WatchContractPanel";
import {
  buildWebhookTrigger,
  DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE,
  saveWebhookRouteSecretIfPresent,
  webhookFieldsFromTrigger,
  WebhookRouteFields,
} from "./WebhookRouteFields";
import type { WebhookRouteFieldsValue } from "./WebhookRouteFields";

type TriggerTab = "cron" | "webhook" | "connector_event";

/** Maps this modal's internal tab id to the wire `RefineTemplateMode` the
 *  backend expects — the two only disagree on the connector tab's name. */
function triggerTabToRefineMode(tab: TriggerTab): RefineTemplateMode {
  return tab === "connector_event" ? "poll_connector" : tab;
}

interface AssignmentEditorModalProps {
  /** Called after a successful create/edit, once the modal has already
   *  closed itself. The modal doesn't know which list/store is showing
   *  assignments, so refreshing that is left to the caller. */
  onSaved?: () => void;
}

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

function pad(n: number): string {
  return n.toString().padStart(2, "0");
}

/** DateTimePicker's wire format is a bare local wall-clock string with no
 *  timezone marker ("YYYY-MM-DDTHH:mm"); `expires_at` on the wire is ISO 8601.
 *  Both conversions happen only at this component's boundary — nothing else
 *  in the modal ever sees a raw ISO string. */
function isoToLocalInput(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function localInputToIso(local: string): string | null {
  if (!local) return null;
  const d = new Date(local);
  if (Number.isNaN(d.getTime())) return null;
  return d.toISOString();
}

/** Builds a full 5-field cron expression that matches exactly the given local
 *  datetime. Used to seed a one-shot Cron trigger from a specific date (e.g.
 *  a calendar cell the caller passes as `seedCronDate`) — combined with
 *  `is_recurring: false` this fires once at that wall-clock time. */
function localInputToCronExpr(local: string): string | null {
  const d = new Date(local);
  if (Number.isNaN(d.getTime())) return null;
  return `${d.getMinutes()} ${d.getHours()} ${d.getDate()} ${d.getMonth() + 1} *`;
}

/** Inverse of `localInputToCronExpr`, for seeding the "Fire at" picker when
 *  editing an existing one-shot Cron assignment. Cron has no year field, so a
 *  round-tripped expression can't tell us which year was originally meant —
 *  we resolve that ambiguity by picking the soonest upcoming occurrence of
 *  the given minute/hour/day/month (this year, or next year if that's
 *  already past), which is the only sensible reading for a trigger that's
 *  meant to fire once in the future.
 *
 *  The stored `cron_expr` is the source of truth here, not the server's
 *  `next_fire_at` — that field goes null once the assignment has fired, been
 *  paused, or expired, and seeding a blank "Fire at" in that case would mean
 *  the next save silently drops the original schedule. `next_fire_at` is
 *  used only as a fallback when the expression itself doesn't parse as a
 *  simple single-instant pattern (e.g. it was hand-edited via CronPicker's
 *  raw mode into something else). */
function cronOneShotToLocalInput(cronExpr: string, fallbackNextFireAt?: string | null): string {
  const parts = cronExpr.trim().split(/\s+/);
  if (parts.length === 5) {
    const [minStr, hourStr, domStr, monStr, dowStr] = parts;
    const minute = Number(minStr);
    const hour = Number(hourStr);
    const day = Number(domStr);
    const month = Number(monStr);
    const isSimpleInstant =
      dowStr === "*" &&
      Number.isInteger(minute) && minute >= 0 && minute <= 59 &&
      Number.isInteger(hour) && hour >= 0 && hour <= 23 &&
      Number.isInteger(day) && day >= 1 && day <= 31 &&
      Number.isInteger(month) && month >= 1 && month <= 12;
    if (isSimpleInstant) {
      const now = new Date();
      let candidate = new Date(now.getFullYear(), month - 1, day, hour, minute, 0, 0);
      if (candidate.getTime() < now.getTime()) {
        candidate = new Date(now.getFullYear() + 1, month - 1, day, hour, minute, 0, 0);
      }
      if (!Number.isNaN(candidate.getTime())) {
        return `${candidate.getFullYear()}-${pad(candidate.getMonth() + 1)}-${pad(candidate.getDate())}T${pad(candidate.getHours())}:${pad(candidate.getMinutes())}`;
      }
    }
  }
  return isoToLocalInput(fallbackNextFireAt);
}

/** Discriminated parse/validate result for the poll-interval field — the
 *  only remaining piece of validated state on the watch tab now that the
 *  raw server/tool/arguments/cursor fields are gone. */
type FieldResult<T> = { ok: true; value: T } | { ok: false; error: string };

/** Locked design decision: every poll on this tab runs a full agent turn
 *  against a live model (this tab only ever builds `AgentWatch` triggers —
 *  see `handleSubmit` below), so the floor here is the much higher
 *  `AgentWatch` floor from `crates/ao-protocol/src/assignment.rs`, not the
 *  cheap deterministic `ConnectorEvent` poll's 60s runtime clamp. Anything
 *  below it is rejected outright rather than silently clamped, and the
 *  backend's own `validate()` enforces the same floor server-side. */

/** Shipped default for a brand-new watch's poll interval. Deliberately NOT
 *  tied to the server-served effective floor (`minPollIntervalSecs` state
 *  below) — a demo-lane override that lowers the floor should let someone
 *  type a smaller number, not silently change what a new watch defaults to. */
const DEFAULT_POLL_INTERVAL_SECS = 900;

/** Fail-closed fallback floor used only when `/system/config` hasn't
 *  answered yet (or answered without a usable value) — matches the backend's
 *  own shipped default (`MIN_AGENT_WATCH_POLL_INTERVAL_SECS`). A missing
 *  served value must never be treated as "no floor"; it must fall back to
 *  the most conservative (highest) known floor, never the most permissive. */
const FALLBACK_MIN_AGENT_WATCH_POLL_INTERVAL_SECS = 900;

/** Renders the "(N minute(s))" parenthetical for a floor that's a clean
 *  minute multiple (e.g. 900 -> " (15 minutes)"); omitted entirely for a
 *  floor like a demo override's 90s that doesn't divide evenly. */
function minutesParenthetical(secs: number): string {
  if (secs <= 0 || secs % 60 !== 0) return "";
  const minutes = secs / 60;
  return ` (${minutes} minute${minutes === 1 ? "" : "s"})`;
}

function validatePollIntervalSecs(text: string, minPollIntervalSecs: number): FieldResult<number> {
  const trimmed = text.trim();
  if (!trimmed) return { ok: false, error: "Poll interval is required." };
  const n = Number(trimmed);
  if (!Number.isFinite(n)) return { ok: false, error: "Poll interval must be a number." };
  if (n < minPollIntervalSecs) {
    return {
      ok: false,
      error: `Poll interval must be at least ${minPollIntervalSecs} seconds${minutesParenthetical(minPollIntervalSecs)}.`,
    };
  }
  return { ok: true, value: Math.floor(n) };
}

const DEFAULTS = {
  name: "",
  instruction: "",
  workingDirectory: "",
  triggerTab: "cron" as TriggerTab,
  cronExpr: "",
  isRecurring: true,
  fireAt: "",
  webhookFields: DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE,
  connectorPollIntervalSecs: String(DEFAULT_POLL_INTERVAL_SECS),
  connectorScope: null as string | null,
  watchContract: null as WatchContract | null,
  watchHealth: null as AssignmentWatchHealth | null,
  extractionTool: null as string | null,
  extractionArgs: null as unknown,
  threadPolicy: "fresh" as AssignmentThreadPolicy,
  expiresAt: "",
  enabled: true,
};

// ---------------------------------------------------------------------------
// Shared styles / small pieces
// ---------------------------------------------------------------------------

const inputClass =
  "w-full h-[40px] px-3 bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[10px] text-[15px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-secondary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors";

// Same look as `inputClass` but no fixed height — for the multi-row
// Instruction textarea, sized by its `rows` attribute instead.
const textareaClass =
  "w-full py-2 px-3 bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[10px] text-[15px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-secondary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors leading-relaxed";

const labelClass = "block text-[15px] font-bold text-[var(--modal-text-primary)]";
const hintClass = "text-[13px] text-[var(--modal-text-secondary)] mt-1.5";

// Small icon button used by the Refine control and its suggestion's
// accept/dismiss actions, next to the Instruction field.
const refineButtonClass =
  "flex items-center justify-center gap-1.5 px-[10px] h-[32px] rounded-[8px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] text-[13px] font-semibold text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed";

function tabClass(active: boolean): string {
  return `px-[14px] py-[8px] text-[13px] font-semibold cursor-pointer whitespace-nowrap transition-colors ${active
    ? "bg-[var(--modal-accent)] text-white"
    : "bg-transparent text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)]"
    }`;
}

/** A real checkbox visually rendered as a Slack-style switch — kept a genuine
 *  `<input type="checkbox">` (just visually hidden) rather than a bare
 *  `<button>` so existing tests can keep asserting on `.checked`. */
function SwitchField({
  label,
  checked,
  onChange,
  testId,
}: {
  label: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
  testId: string;
}) {
  return (
    <label className="inline-flex items-center gap-3 cursor-pointer select-none">
      <span className={labelClass}>{label}</span>
      <span className="relative inline-block w-[42px] h-[24px]">
        <input
          type="checkbox"
          checked={checked}
          onChange={(e) => onChange(e.target.checked)}
          data-testid={testId}
          className="peer sr-only"
        />
        <span className="absolute inset-0 rounded-full bg-[var(--modal-border-primary)] peer-checked:bg-[var(--success)] transition-colors pointer-events-none" />
        <span className="absolute top-[2px] left-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform peer-checked:translate-x-[18px] pointer-events-none" />
      </span>
    </label>
  );
}

function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-[8px]">
      <label className={labelClass}>{label}</label>
      {children}
      {hint && <p className={hintClass}>{hint}</p>}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/** Trigger-aware create/edit modal for Assignments, driven entirely by
 *  assignmentEditorModalStore. Renders null when the store is empty. Mounted
 *  once at the app shell root so any view can open it; pages that list
 *  assignments watch the store's `savedAt` counter to know when to refetch. */
export function AssignmentEditorModal({ onSaved }: AssignmentEditorModalProps) {
  const editorState = useAssignmentEditorModalStore((s) => s.state);
  const close = useAssignmentEditorModalStore((s) => s.close);
  const markSaved = useAssignmentEditorModalStore((s) => s.markSaved);
  const selectAgent = useAssignmentEditorModalStore((s) => s.selectAgent);
  const navigate = useNavigate();

  // Store-backed CRUD (instead of calling lib/api directly) so chatStore's
  // `assignmentsByAgent` cache — the same cache the left-side tiles panel and
  // ChatHeader's Assignments badge read — stays correct with no separate
  // refetch after a save/delete here.
  const storeCreateAssignment = useChatStore((s) => s.createAssignment);
  const storeUpdateAssignment = useChatStore((s) => s.updateAssignment);
  const storeDeleteAssignment = useChatStore((s) => s.deleteAssignment);
  const storeTriggerAssignment = useChatStore((s) => s.triggerAssignment);

  // Owner picker — only relevant for the "no agent yet" create entry points
  // (e.g. the sidebar's "New assignment" button, or a calendar "+" click),
  // mirroring the pre-convergence ScheduledTaskModal's in-modal agent select.
  const agents = useChatStore((s) => s.agents);
  const fetchAgents = useChatStore((s) => s.fetchAgents);
  useEffect(() => {
    if (editorState && agents.length === 0) fetchAgents();
  }, [editorState, agents.length, fetchAgents]);
  const agentOptions = useMemo(() => agents.filter((a) => !a.owning_team_id), [agents]);

  const [name, setName] = useState(DEFAULTS.name);
  const [instruction, setInstruction] = useState(DEFAULTS.instruction);
  const [workingDirectory, setWorkingDirectory] = useState(DEFAULTS.workingDirectory);
  const [triggerTab, setTriggerTab] = useState<TriggerTab>(DEFAULTS.triggerTab);
  const [cronExpr, setCronExpr] = useState(DEFAULTS.cronExpr);
  const [isRecurring, setIsRecurring] = useState(DEFAULTS.isRecurring);
  const [fireAt, setFireAt] = useState(DEFAULTS.fireAt);
  const [webhookFields, setWebhookFields] = useState<WebhookRouteFieldsValue>(DEFAULTS.webhookFields);
  const [connectorPollIntervalSecs, setConnectorPollIntervalSecs] = useState(DEFAULTS.connectorPollIntervalSecs);
  const [connectorScope, setConnectorScope] = useState<string | null>(DEFAULTS.connectorScope);
  const [watchContract, setWatchContract] = useState<WatchContract | null>(DEFAULTS.watchContract);
  const [watchHealth, setWatchHealth] = useState<AssignmentWatchHealth | null>(DEFAULTS.watchHealth);
  const [extractionTool, setExtractionTool] = useState<string | null>(DEFAULTS.extractionTool);
  const [extractionArgs, setExtractionArgs] = useState<unknown>(DEFAULTS.extractionArgs);
  const [mcpServers, setMcpServers] = useState<McpServerStatus[]>([]);
  const [minPollIntervalSecs, setMinPollIntervalSecs] = useState<number>(
    FALLBACK_MIN_AGENT_WATCH_POLL_INTERVAL_SECS,
  );
  const [threadPolicy, setThreadPolicy] = useState<AssignmentThreadPolicy>(DEFAULTS.threadPolicy);
  const [expiresAt, setExpiresAt] = useState(DEFAULTS.expiresAt);
  const [enabled, setEnabled] = useState(DEFAULTS.enabled);

  const [loadingExisting, setLoadingExisting] = useState(false);
  const [busy, setBusy] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [triggering, setTriggering] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [refining, setRefining] = useState(false);
  const [refineError, setRefineError] = useState<string | null>(null);
  const [refineSuggestion, setRefineSuggestion] = useState<string | null>(null);

  // Populates the Watch tab's connector dropdown — same `listMcpServers` +
  // `state === "connected"` source BindingPicker uses, refetched each time
  // the modal is opened so a connector authorized mid-session shows up.
  useEffect(() => {
    if (!editorState) return;
    let cancelled = false;
    listMcpServers()
      .then((list) => {
        if (!cancelled) setMcpServers(list);
      })
      .catch(() => {
        if (!cancelled) setMcpServers([]);
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editorState?.mode, editorState?.assignmentId, editorState?.seedCronDate]);
  const connectedMcpServers = useMemo(() => mcpServers.filter((s) => s.state === "connected"), [mcpServers]);

  // Server-served effective AgentWatch poll-interval floor (may be lowered
  // below the shipped default by a demo/local env override — see
  // `effective_min_agent_watch_poll_interval_secs` in
  // `crates/ao-protocol/src/assignment.rs`). Fails closed: any fetch error,
  // or a missing/non-finite value in the response, leaves
  // `minPollIntervalSecs` at its fallback (the shipped default) rather than
  // silently accepting a lower or unbounded floor.
  useEffect(() => {
    if (!editorState) return;
    let cancelled = false;
    getAppConfig()
      .then((config) => {
        if (cancelled) return;
        const value = config.min_agent_watch_poll_interval_secs;
        setMinPollIntervalSecs(
          Number.isFinite(value) ? value : FALLBACK_MIN_AGENT_WATCH_POLL_INTERVAL_SECS,
        );
      })
      .catch(() => {
        if (!cancelled) setMinPollIntervalSecs(FALLBACK_MIN_AGENT_WATCH_POLL_INTERVAL_SECS);
      });
    return () => {
      cancelled = true;
    };
  }, [editorState]);

  const resetToDefaults = useCallback((seedCronDate?: string) => {
    setName(DEFAULTS.name);
    setInstruction(DEFAULTS.instruction);
    setWorkingDirectory(DEFAULTS.workingDirectory);
    setWebhookFields(DEFAULTS.webhookFields);
    setConnectorPollIntervalSecs(DEFAULTS.connectorPollIntervalSecs);
    setConnectorScope(DEFAULTS.connectorScope);
    setWatchContract(DEFAULTS.watchContract);
    setWatchHealth(DEFAULTS.watchHealth);
    setExtractionTool(DEFAULTS.extractionTool);
    setExtractionArgs(DEFAULTS.extractionArgs);
    setThreadPolicy(DEFAULTS.threadPolicy);
    setExpiresAt(DEFAULTS.expiresAt);
    setEnabled(DEFAULTS.enabled);
    setError(null);

    if (seedCronDate) {
      // seedCronDate is a bare "YYYY-MM-DD" (a calendar cell, not a specific
      // time) — default to 9 AM local, same as the pre-convergence modal,
      // rather than treating it as a full datetime (midnight UTC on a
      // date-only string parses to the wrong local day/time).
      setTriggerTab("cron");
      setIsRecurring(false);
      setCronExpr(DEFAULTS.cronExpr);
      setFireAt(`${seedCronDate}T09:00`);
    } else {
      setTriggerTab(DEFAULTS.triggerTab);
      setCronExpr(DEFAULTS.cronExpr);
      setIsRecurring(DEFAULTS.isRecurring);
      setFireAt(DEFAULTS.fireAt);
    }
  }, []);

  const applyAssignment = useCallback((assignment: Assignment) => {
    setName(assignment.name);
    setWorkingDirectory(assignment.working_directory ?? "");
    setThreadPolicy(assignment.thread_policy);
    setExpiresAt(isoToLocalInput(assignment.expires_at));
    setEnabled(assignment.enabled);
    if (assignment.trigger.type === "Cron") {
      setInstruction(assignment.instruction);
      setTriggerTab("cron");
      setIsRecurring(assignment.trigger.is_recurring);
      setCronExpr(assignment.trigger.cron_expr);
      setFireAt(
        assignment.trigger.is_recurring
          ? DEFAULTS.fireAt
          : cronOneShotToLocalInput(assignment.trigger.cron_expr, assignment.next_fire_at),
      );
      setWebhookFields(DEFAULTS.webhookFields);
      setConnectorPollIntervalSecs(DEFAULTS.connectorPollIntervalSecs);
      setConnectorScope(DEFAULTS.connectorScope);
      setWatchContract(DEFAULTS.watchContract);
      setWatchHealth(DEFAULTS.watchHealth);
      setExtractionTool(DEFAULTS.extractionTool);
      setExtractionArgs(DEFAULTS.extractionArgs);
    } else if (assignment.trigger.type === "Webhook") {
      // The trigger's own `prompt_template` (when set) is what actually
      // drove the rendered instruction at fire time — see
      // `webhook_dispatch::dispatch_webhook_route` — so prefer it here over
      // the static `instruction` the wire payload also carries, falling
      // back to that static instruction only for a route with no template.
      const template = assignment.trigger.prompt_template;
      setInstruction(template && template.trim() ? template : assignment.instruction);
      setTriggerTab("webhook");
      setWebhookFields(webhookFieldsFromTrigger(assignment.trigger));
      setCronExpr("");
      setIsRecurring(true);
      setFireAt(DEFAULTS.fireAt);
      setConnectorPollIntervalSecs(DEFAULTS.connectorPollIntervalSecs);
      setConnectorScope(DEFAULTS.connectorScope);
      setWatchContract(DEFAULTS.watchContract);
      setWatchHealth(DEFAULTS.watchHealth);
      setExtractionTool(DEFAULTS.extractionTool);
      setExtractionArgs(DEFAULTS.extractionArgs);
    } else if (assignment.trigger.type === "AgentWatch") {
      // Same precedent as Webhook's `prompt_template` above: the trigger's
      // own `instruction` is what the detect loop actually evaluates each
      // poll (see `ao_engine::schedule_runner::tick_agent_watches`), so
      // prefer it over the static `instruction` field it's normally kept in
      // sync with.
      setInstruction(assignment.trigger.instruction);
      setTriggerTab("connector_event");
      setCronExpr("");
      setIsRecurring(true);
      setFireAt(DEFAULTS.fireAt);
      setWebhookFields(DEFAULTS.webhookFields);
      setConnectorPollIntervalSecs(String(assignment.trigger.poll_interval_secs));
      setConnectorScope(assignment.trigger.connector_scope ?? DEFAULTS.connectorScope);
      setWatchContract(assignment.trigger.contract ?? DEFAULTS.watchContract);
      setWatchHealth(assignment.watch_health ?? DEFAULTS.watchHealth);
      setExtractionTool(assignment.trigger.extraction_tool ?? DEFAULTS.extractionTool);
      setExtractionArgs(assignment.trigger.extraction_args ?? DEFAULTS.extractionArgs);
    } else {
      // Legacy `ConnectorEvent` row from before this tab collapsed to
      // instruction + cadence — its raw server/tool/arguments/cursor fields
      // have no editor here anymore, so load only what still maps cleanly
      // (the static instruction + poll interval) rather than crashing.
      // Saving this assignment rewrites its trigger as `AgentWatch`, same as
      // any other row on this tab. Its `server_name` carries over into
      // `connector_scope` so the upgrade doesn't silently widen an
      // already-scoped watch to every connector.
      setInstruction(assignment.instruction);
      setTriggerTab("connector_event");
      setCronExpr("");
      setIsRecurring(true);
      setFireAt(DEFAULTS.fireAt);
      setWebhookFields(DEFAULTS.webhookFields);
      setConnectorPollIntervalSecs(String(assignment.trigger.poll_interval_secs));
      setConnectorScope(assignment.trigger.server_name ?? DEFAULTS.connectorScope);
      setWatchContract(DEFAULTS.watchContract);
      setWatchHealth(DEFAULTS.watchHealth);
      setExtractionTool(DEFAULTS.extractionTool);
      setExtractionArgs(DEFAULTS.extractionArgs);
    }
  }, []);

  // Load (edit) or reset (create) whenever what's being edited changes.
  useEffect(() => {
    if (!editorState) return;
    setRefineError(null);
    setRefineSuggestion(null);
    if (editorState.mode === "edit" && editorState.assignmentId) {
      const assignmentId = editorState.assignmentId;
      setLoadingExisting(true);
      setError(null);
      getAssignment(assignmentId)
        .then((assignment) => applyAssignment(assignment))
        .catch(() => setError("Couldn't load this assignment."))
        .finally(() => setLoadingExisting(false));
    } else {
      resetToDefaults(editorState.seedCronDate);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editorState?.mode, editorState?.assignmentId, editorState?.seedCronDate]);

  // `watchHealth`/`watchContract` reflect a live watch's background polling,
  // which keeps advancing (and racking up model calls) the whole time this
  // modal sits open on the Watch tab — the load effect above only fetches
  // once, so without this the panel would freeze at whatever it first saw
  // and under-report a cost figure that has since moved on. Refetches just
  // these two fields rather than re-running `applyAssignment`, so it never
  // clobbers instruction/name/etc. edits the user has in progress.
  const isEditingAgentWatch = editorState?.mode === "edit" && triggerTab === "connector_event";
  useEffect(() => {
    if (!isEditingAgentWatch || !editorState?.assignmentId) return;
    const assignmentId = editorState.assignmentId;
    const id = window.setInterval(() => {
      getAssignment(assignmentId)
        .then((assignment) => {
          if (assignment.trigger.type === "AgentWatch") {
            setWatchHealth(assignment.watch_health ?? DEFAULTS.watchHealth);
            setWatchContract(assignment.trigger.contract ?? DEFAULTS.watchContract);
          }
        })
        .catch(() => {
          // Best-effort refresh — keep showing the last known-good health
          // rather than erroring the whole modal over a missed poll.
        });
    }, 5000);
    return () => window.clearInterval(id);
  }, [isEditingAgentWatch, editorState?.assignmentId]);

  useEffect(() => {
    if (!editorState) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [editorState, close]);

  // A refine suggestion is scoped to the mode it was generated for — drop it
  // (and any stale error) when the user switches trigger tabs.
  useEffect(() => {
    setRefineError(null);
    setRefineSuggestion(null);
  }, [triggerTab]);

  if (!editorState) return null;

  const pickingOwner = editorState.mode === "create" && !editorState.agentId;

  const connectorPollIntervalResult = validatePollIntervalSecs(connectorPollIntervalSecs, minPollIntervalSecs);

  const triggerValid =
    triggerTab === "cron"
      ? isRecurring
        ? cronExpr.trim().length > 0
        : fireAt.trim().length > 0 && localInputToCronExpr(fireAt) !== null
      : triggerTab === "webhook"
        ? true
        : connectorPollIntervalResult.ok;

  const canSubmit =
    !pickingOwner &&
    name.trim().length > 0 &&
    instruction.trim().length > 0 &&
    triggerValid &&
    !loadingExisting &&
    !busy &&
    !triggering;

  const handleBrowse = async () => {
    try {
      const selected = await tauriOpen({ directory: true, multiple: false });
      if (!selected) return;
      setWorkingDirectory(selected as string);
    } catch {
      // Dialog cancelled or unavailable (e.g. web/test environment).
    }
  };

  const handleDelete = async () => {
    if (editorState.mode !== "edit" || !editorState.assignmentId) return;
    setDeleting(true);
    setError(null);
    try {
      await storeDeleteAssignment(editorState.assignmentId);
      close();
      markSaved();
      onSaved?.();
    } catch {
      setError("Delete failed. Please try again.");
    } finally {
      setDeleting(false);
    }
  };

  // Manual "fire now" — the one trigger path that carries an explicit
  // foreground intent, unlike a cron/webhook fire, so unlike those this
  // navigates the operator straight to the run's thread instead of leaving
  // them wherever they were (see the Assignments/Delegate thread-navigation
  // design: background fires must never steal focus, a manual trigger
  // always should). `fire_assignment` (ao-engine/src/assignment_runner.rs)
  // resolves and persists `display_thread_id` synchronously before this
  // endpoint returns — even for a `Fresh` thread policy that mints a brand
  // new thread — so `run.thread_id` is already the real id here, no need to
  // wait for the `thread_created` SSE event. `switchToThread` runs before
  // `navigate` (not after) so ChatView mounts already pointed at the run's
  // thread, same ordering HomeSidebar's own cross-page thread navigation
  // uses.
  const handleRunNow = async () => {
    if (editorState.mode !== "edit" || !editorState.assignmentId || !editorState.agentId) return;
    const agentId = editorState.agentId;
    setTriggering(true);
    setError(null);
    try {
      const run = await storeTriggerAssignment(editorState.assignmentId);
      if (run.thread_id) {
        await switchToThread(agentId, run.thread_id);
      }
      close();
      navigate(`/chat/${agentId}`);
    } catch {
      setError("Trigger failed. Please try again.");
    } finally {
      setTriggering(false);
    }
  };

  const handleRefine = async () => {
    if (!editorState.agentId) return;
    setRefining(true);
    setRefineError(null);
    setRefineSuggestion(null);
    try {
      const result = await refineTemplate({
        agent_id: editorState.agentId,
        prompt_template: instruction,
        mode: triggerTabToRefineMode(triggerTab),
      });
      setRefineSuggestion(result.refined_template);
    } catch (err) {
      setRefineError(err instanceof Error ? err.message : "Refine failed.");
    } finally {
      setRefining(false);
    }
  };

  const acceptRefineSuggestion = () => {
    if (refineSuggestion == null) return;
    setInstruction(refineSuggestion);
    setRefineSuggestion(null);
  };

  const handleSubmit = async () => {
    if (!canSubmit) return;

    let trigger: AssignmentTrigger;
    if (triggerTab === "cron") {
      const expr = isRecurring ? cronExpr.trim() : (localInputToCronExpr(fireAt) ?? "");
      trigger = { type: "Cron", cron_expr: expr, is_recurring: isRecurring };
    } else if (triggerTab === "webhook") {
      trigger = buildWebhookTrigger(webhookFields, instruction);
    } else {
      // canSubmit already guarantees this is ok.
      if (!connectorPollIntervalResult.ok) return;
      trigger = {
        type: "AgentWatch",
        instruction: instruction.trim(),
        poll_interval_secs: connectorPollIntervalResult.value,
        connector_scope: connectorScope,
      };
    }

    const common = {
      name: name.trim(),
      instruction: instruction.trim(),
      working_directory: workingDirectory.trim() ? workingDirectory.trim() : null,
      trigger,
      bindings: [],
      thread_policy: threadPolicy,
      enabled,
      expires_at: localInputToIso(expiresAt),
    };

    setBusy(true);
    setError(null);
    try {
      let savedAssignment: Assignment;
      if (editorState.mode === "create") {
        if (!editorState.agentId) {
          setError("No agent selected for this assignment.");
          return;
        }
        const req: CreateAssignmentRequest = common;
        savedAssignment = await storeCreateAssignment(editorState.agentId, req);
      } else {
        if (!editorState.assignmentId) {
          setError("Missing assignment id.");
          return;
        }
        const patch: PatchAssignmentRequest = common;
        savedAssignment = await storeUpdateAssignment(editorState.assignmentId, patch);
      }

      if (triggerTab === "webhook") {
        try {
          await saveWebhookRouteSecretIfPresent(webhookFields);
        } catch {
          // The assignment itself saved fine; only the secret write failed.
          // Pivot a fresh "create" into "edit" pointing at the row that now
          // exists, so clicking Save again retries just the secret via a
          // PATCH instead of creating a duplicate assignment.
          if (editorState.mode === "create" && editorState.agentId) {
            useAssignmentEditorModalStore.getState().openEdit(editorState.agentId, savedAssignment.id);
          }
          markSaved();
          onSaved?.();
          setError("Assignment saved, but the webhook secret failed to save — click Save again to retry just the secret.");
          return;
        }
      }

      close();
      markSaved();
      onSaved?.();
    } catch {
      setError("Save failed. Please try again.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <AnimatePresence>
      <div className="fixed inset-0 z-[310] flex items-center justify-center p-4" data-testid="assignment-editor-modal">
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.15 }}
          className="absolute inset-0 bg-black/40"
          onClick={() => !busy && !deleting && !triggering && close()}
        />

        <motion.div
          initial={{ opacity: 0, scale: 0.96 }}
          animate={{ opacity: 1, scale: 1 }}
          exit={{ opacity: 0, scale: 0.96 }}
          transition={{ duration: 0.15, ease: "easeOut" }}
          className="scheduled-task-modal relative w-full max-w-[900px] h-[80vh] max-h-[680px] bg-[var(--modal-bg)] rounded-[16px] flex flex-row overflow-hidden border border-transparent dark:border-[var(--modal-border-secondary)]"
          style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
        >
          {/* Left panel — every other assignment belonging to this agent. */}
          <AssignmentAgentTiles
            agentId={editorState.agentId}
            selectedAssignmentId={editorState.mode === "edit" ? editorState.assignmentId : null}
            onSelect={(assignmentId) => {
              if (editorState.agentId) {
                useAssignmentEditorModalStore.getState().openEdit(editorState.agentId, assignmentId);
              }
            }}
            onNew={() => useAssignmentEditorModalStore.getState().openCreate(editorState.agentId ?? undefined)}
          />

          {/* Right panel — the form. */}
          <div className="flex-1 flex flex-col min-w-0">
            <div className="flex items-center justify-between px-[18px] py-[14px] shrink-0">
              <div className="flex items-center gap-2.5">
                <CalendarClock size={20} className="text-[var(--modal-text-secondary)]" />
                <span className="text-[20px] font-bold text-[var(--modal-text-primary)] tracking-tight">
                  {editorState.mode === "create" ? "New assignment" : "Edit assignment"}
                </span>
              </div>
              <button
                type="button"
                onClick={() => !busy && !deleting && !triggering && close()}
                data-testid="assignment-editor-close"
                className="p-2 rounded-md text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                aria-label="Close"
              >
                <X strokeWidth={2} size={20} />
              </button>
            </div>

            <div className="flex-1 min-h-0 overflow-y-auto px-[18px] py-[16px] flex flex-col gap-[16px]">
              {loadingExisting ? (
                <div className="text-[13px] text-[var(--modal-text-secondary)]" data-testid="assignment-editor-loading">
                  Loading…
                </div>
              ) : (
                <>
                  {pickingOwner && (
                    <Field label="Agent">
                      {agentOptions.length === 0 ? (
                        <p className="text-[13px] text-[var(--modal-text-secondary)]">No agents available.</p>
                      ) : (
                        <select
                          value={editorState.agentId ?? ""}
                          onChange={(e) => {
                            if (e.target.value) selectAgent(e.target.value);
                          }}
                          data-testid="assignment-editor-agent-select"
                          className={`${inputClass} appearance-none cursor-pointer`}
                        >
                          <option value="" disabled>
                            Select an agent...
                          </option>
                          {agentOptions.map((a) => (
                            <option key={a.agent_id} value={a.agent_id}>
                              {a.emoji ? `${a.emoji} ` : ""}
                              {a.name}
                            </option>
                          ))}
                        </select>
                      )}
                      <p className={hintClass}>Choose which agent this assignment belongs to.</p>
                    </Field>
                  )}

                  <fieldset
                    disabled={pickingOwner}
                    className={`flex flex-col gap-[16px] border-0 m-0 p-0 min-w-0 ${pickingOwner ? "opacity-40 pointer-events-none" : ""
                      }`}
                  >
                    <Field label="Name" hint="Shown in the assignments list and calendar instead of the instruction.">
                      <input
                        value={name}
                        onChange={(e) => setName(e.target.value)}
                        data-testid="assignment-editor-name-input"
                        placeholder="Daily digest"
                        className={inputClass}
                      />
                    </Field>

                    <Field label="Instruction">
                      <textarea
                        value={instruction}
                        onChange={(e) => setInstruction(e.target.value)}
                        data-testid="assignment-editor-instruction-input"
                        placeholder="What should the agent do when this fires?"
                        rows={3}
                        className={`${textareaClass} resize-y`}
                      />
                      {triggerTab === "webhook" && (
                        <p className={hintClass}>
                          Reference payload fields with <code>{"{dot.path}"}</code> (e.g. <code>{"{pull_request.title}"}</code>) —
                          rendered against the inbound event. The <code>{"{__raw__}"}</code> escape hatch (see Advanced below)
                          dumps the full payload for anything a named field doesn't cover.
                        </p>
                      )}
                      {triggerTab === "connector_event" && (
                        <p className={hintClass}>
                          Describe both what to watch for and what to do about it (e.g. "when a new email
                          from finance arrives, summarize it") — the agent checks for that on the cadence
                          below and decides for itself which tools to call.
                        </p>
                      )}
                      <div className="flex items-center gap-2 mt-[8px]">
                        <button
                          type="button"
                          onClick={() => void handleRefine()}
                          disabled={refining || !editorState.agentId}
                          title={!editorState.agentId ? "Pick an owning agent first." : undefined}
                          data-testid="assignment-editor-instruction-refine"
                          className={refineButtonClass}
                        >
                          {refining ? <Loader2 size={13} className="animate-spin" /> : <Sparkles size={13} />}
                          Refine
                        </button>
                        {refineError && (
                          <span className="text-[13px] text-[var(--error)]" data-testid="assignment-editor-instruction-refine-error">
                            {refineError}
                          </span>
                        )}
                      </div>
                      {refineSuggestion != null && (
                        <div
                          className="flex flex-col gap-[6px] mt-[8px] p-[10px] rounded-[8px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)]"
                          data-testid="assignment-editor-instruction-refine-suggestion"
                        >
                          <span className="text-[13px] font-semibold text-[var(--modal-text-primary)]">Suggested rewrite</span>
                          <pre className="text-[12px] font-mono whitespace-pre-wrap text-[var(--modal-text-secondary)]">{refineSuggestion}</pre>
                          <div className="flex gap-2">
                            <button
                              type="button"
                              onClick={acceptRefineSuggestion}
                              data-testid="assignment-editor-instruction-refine-accept"
                              className={refineButtonClass}
                            >
                              Accept
                            </button>
                            <button
                              type="button"
                              onClick={() => setRefineSuggestion(null)}
                              data-testid="assignment-editor-instruction-refine-dismiss"
                              className={refineButtonClass}
                            >
                              Dismiss
                            </button>
                          </div>
                        </div>
                      )}
                    </Field>

                    <Field label="Trigger">
                      <div className="inline-flex rounded-[8px] border border-[var(--modal-border-primary)] overflow-hidden self-start mb-[8px]">
                        <button
                          type="button"
                          onClick={() => setTriggerTab("cron")}
                          data-testid="assignment-editor-trigger-cron-tab"
                          className={tabClass(triggerTab === "cron")}
                        >
                          Cron
                        </button>
                        <button
                          type="button"
                          onClick={() => setTriggerTab("webhook")}
                          data-testid="assignment-editor-trigger-webhook-tab"
                          className={tabClass(triggerTab === "webhook")}
                        >
                          Webhook
                        </button>
                        <button
                          type="button"
                          onClick={() => setTriggerTab("connector_event")}
                          data-testid="assignment-editor-trigger-connector_event-tab"
                          className={tabClass(triggerTab === "connector_event")}
                        >
                          Watch
                        </button>
                      </div>

                      {triggerTab === "cron" ? (
                        <div className="flex flex-col gap-[14px]">
                          <SwitchField
                            label="Recurring"
                            checked={isRecurring}
                            onChange={setIsRecurring}
                            testId="assignment-editor-recurring-input"
                          />

                          {isRecurring ? (
                            <div className="flex flex-col gap-[8px]">
                              <label className={labelClass}>Schedule</label>
                              <CronPicker value={cronExpr} onChange={setCronExpr} />
                            </div>
                          ) : (
                            <div className="flex flex-col gap-[8px]">
                              <label className={labelClass}>Fire at</label>
                              <DateTimePicker
                                value={fireAt}
                                onChange={setFireAt}
                                placeholder="Select fire date & time"
                              />
                              <p className={hintClass}>When this one-shot assignment should fire.</p>
                            </div>
                          )}

                          {isRecurring && (
                            <div className="flex flex-col gap-[8px]" data-testid="assignment-editor-expires-at-field">
                              <label className={labelClass}>
                                Expires at{" "}
                                <span className="text-[var(--modal-text-secondary)] font-normal">(optional)</span>
                              </label>
                              <DateTimePicker value={expiresAt} onChange={setExpiresAt} placeholder="Never" />
                              <p className={hintClass}>The assignment stops firing after this time.</p>
                            </div>
                          )}
                        </div>
                      ) : triggerTab === "webhook" ? (
                        <div className="flex flex-col gap-[14px]">
                          <WebhookRouteFields value={webhookFields} onChange={setWebhookFields} promptTemplate={instruction} />
                          <div className="flex flex-col gap-[8px]" data-testid="assignment-editor-expires-at-field">
                            <label className={labelClass}>
                              Expires at{" "}
                              <span className="text-[var(--modal-text-secondary)] font-normal">(optional)</span>
                            </label>
                            <DateTimePicker value={expiresAt} onChange={setExpiresAt} placeholder="Never" />
                            <p className={hintClass}>The assignment stops firing after this time.</p>
                          </div>
                        </div>
                      ) : (
                        <div className="flex flex-col gap-[10px]">
                          <p className={hintClass}>
                            The agent checks the Instruction above on this cadence, deciding for itself which
                            tools to call to find out whether the thing you described has happened — no
                            server, tool, or argument setup needed here.
                          </p>
                          <div className="flex flex-col gap-[8px]">
                            <label className={labelClass}>Connector</label>
                            <select
                              value={connectorScope ?? ""}
                              onChange={(e) => setConnectorScope(e.target.value ? e.target.value : null)}
                              data-testid="assignment-editor-connector-scope-select"
                              className={`${inputClass} appearance-none cursor-pointer`}
                            >
                              <option value="">All connectors</option>
                              {connectedMcpServers.map((server) => (
                                <option key={server.name} value={server.name}>
                                  {server.name}
                                </option>
                              ))}
                            </select>
                            <p className={hintClass}>
                              Restricts the watch agent to this connector's tools only. Leave on "All
                              connectors" to let it use every configured MCP tool.
                            </p>
                          </div>
                          <div className="flex flex-col gap-[8px]">
                            <label className={labelClass}>Check every (seconds)</label>
                            <input
                              type="number"
                              min={minPollIntervalSecs}
                              value={connectorPollIntervalSecs}
                              onChange={(e) => setConnectorPollIntervalSecs(e.target.value)}
                              data-testid="assignment-editor-connector-poll-interval-input"
                              className={inputClass}
                            />
                            {!connectorPollIntervalResult.ok && (
                              <span
                                className="text-[13px] text-[var(--error)]"
                                data-testid="assignment-editor-connector-poll-interval-error"
                              >
                                {connectorPollIntervalResult.error}
                              </span>
                            )}
                          </div>
                          {editorState.mode === "edit" && (
                            <div className="flex flex-col gap-[8px]">
                              <label className={labelClass}>Watch contract</label>
                              <WatchContractPanel
                                contract={watchContract}
                                watchHealth={watchHealth}
                                extractionTool={extractionTool}
                                extractionArgs={extractionArgs}
                              />
                            </div>
                          )}
                          <div className="flex flex-col gap-[8px]" data-testid="assignment-editor-expires-at-field">
                            <label className={labelClass}>
                              Expires at{" "}
                              <span className="text-[var(--modal-text-secondary)] font-normal">(optional)</span>
                            </label>
                            <DateTimePicker value={expiresAt} onChange={setExpiresAt} placeholder="Never" />
                            <p className={hintClass}>The assignment stops firing after this time.</p>
                          </div>
                        </div>
                      )}
                    </Field>

                    <Field
                      label="Working directory"
                      hint="Override the agent's default working directory for this run."
                    >
                      <div className="flex gap-2">
                        <input
                          value={workingDirectory}
                          onChange={(e) => setWorkingDirectory(e.target.value)}
                          data-testid="assignment-editor-working-directory-input"
                          placeholder="Optional path override"
                          className={`flex-1 ${inputClass} font-mono text-[14px]`}
                        />
                        <button
                          type="button"
                          onClick={() => void handleBrowse()}
                          data-testid="assignment-editor-browse-working-directory"
                          className="flex items-center justify-center gap-1.5 px-4 h-[40px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] text-[15px] font-bold text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                        >
                          <FolderOpen size={16} />
                          Browse
                        </button>
                      </div>
                    </Field>

                    <Field label="Thread policy">
                      <div className="inline-flex rounded-[8px] border border-[var(--modal-border-primary)] overflow-hidden self-start mb-[6px] flex-wrap">
                        {THREAD_POLICY_OPTIONS.map((opt) => (
                          <button
                            key={opt.value}
                            type="button"
                            onClick={() => setThreadPolicy(opt.value)}
                            data-testid={`assignment-editor-thread-policy-${opt.value}`}
                            className={tabClass(threadPolicy === opt.value)}
                          >
                            {opt.label}
                          </button>
                        ))}
                      </div>
                      <span className="text-[13px] text-[var(--modal-text-secondary)]">
                        {THREAD_POLICY_OPTIONS.find((opt) => opt.value === threadPolicy)?.caption}
                      </span>
                    </Field>

                    {editorState.mode === "edit" && (
                      <SwitchField
                        label="Enabled"
                        checked={enabled}
                        onChange={setEnabled}
                        testId="assignment-editor-enabled-input"
                      />
                    )}
                  </fieldset>

                  {error && (
                    <div className="text-[14px] text-[var(--error)] font-medium" data-testid="assignment-editor-error">
                      {error}
                    </div>
                  )}
                </>
              )}
            </div>

            <div className="px-3 py-3 flex items-center justify-between shrink-0">
              <div className="flex items-center gap-[4px]">
                {editorState.mode === "edit" && (
                  <>
                    <button
                      type="button"
                      onClick={() => void handleRunNow()}
                      disabled={triggering || busy || deleting || !enabled}
                      title={!enabled ? "Enable this assignment first." : "Fire this assignment now and open its thread."}
                      data-testid="assignment-editor-run-now"
                      className="flex items-center gap-1.5 px-4 py-2 rounded-[8px] text-[var(--modal-text-primary)] text-[14px] font-bold hover:bg-[var(--modal-bg-hover)] transition-colors disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
                    >
                      {triggering ? <Loader2 size={14} className="animate-spin" /> : <Play size={14} />}
                      Run now
                    </button>
                    <button
                      type="button"
                      onClick={() => void handleDelete()}
                      disabled={deleting || busy || triggering}
                      data-testid="assignment-editor-delete"
                      className="flex items-center gap-1.5 px-4 py-2 rounded-[8px] text-[var(--error)] text-[14px] font-bold hover:bg-[var(--error-bg)] transition-colors disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
                    >
                      {deleting ? <Loader2 size={14} className="animate-spin" /> : <Trash2 size={14} />}
                      Delete
                    </button>
                  </>
                )}
              </div>
              <div className="flex items-center gap-[8px]">
                <button
                  type="button"
                  onClick={close}
                  className="px-[12px] py-2 rounded-[8px] text-[14px] font-bold text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                >
                  Cancel
                </button>
                <button
                  type="button"
                  onClick={() => void handleSubmit()}
                  disabled={!canSubmit}
                  data-testid="assignment-editor-submit"
                  className="flex items-center justify-center gap-2 px-6 py-2 h-[40px] rounded-[8px] bg-[var(--success)] text-white text-[15px] font-bold hover:brightness-110 transition-all disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
                >
                  {busy ? <Loader2 size={16} className="animate-spin" /> : editorState.mode === "create" ? "Create" : "Save"}
                </button>
              </div>
            </div>
          </div>
        </motion.div>
      </div>
    </AnimatePresence>
  );
}
