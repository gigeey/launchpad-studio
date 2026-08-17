import { useEffect, useLayoutEffect, useRef, useCallback, useState, useMemo } from "react";
import { Bookmark, Calendar, ChevronDown, ListChecks, Search, ArrowRight, AlertCircle } from "lucide-react";
import { motion, AnimatePresence } from "framer-motion";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useChatStore, useStreamingText, useActiveToolCalls, useInFlightThinking, useStreamingThreadId, inFlightKey, pendingFormForThread } from "../../stores/chatStore";
import { useBookmarkStore } from "../../stores/bookmarkStore";
import { useSearchStore } from "../../stores/searchStore";
import { useArtifactStore, parseArtifactWriteOutput, type ArtifactCardStub } from "../../stores/artifactStore";
import { MessageBubble, type CoalescedSegment } from "./MessageBubble";
import { StreamingMessage } from "./StreamingMessage";
import { DateSeparator, formatDateLabel, utcToLocalDate } from "./DateSeparator";
import { ForkDivider } from "./ForkDivider";
import { isAgentMessage, isScheduleMessage, isUserMessage, type TranscriptEntry } from "../../types/api";
import { useResolvedAgent } from "../../lib/agents/resolveAgent";
import {
  submitAsyncFormAnswer,
  dismissAsyncForm,
  submitAsyncFormAnswerForProject,
  dismissAsyncFormForProject,
} from "../../lib/api";
import { AsyncFormAnswerBubble } from "./FormAnswerBubble";
import { AskUserQuestionForm } from "./AskUserQuestionForm";
import { AsyncFormRequestCard } from "./AsyncFormRequestCard";
import { FormDismissedIndicator } from "./FormDismissedIndicator";
import { FormWithdrawnIndicator } from "./FormWithdrawnIndicator";
import { FormActionIndicator } from "./FormActionIndicator";
import { stripMcpPrefix } from "./toolCallLabel";
import type { FormRequestPayload, FormAnswerMap, FormFieldDef, FormOptionDef, AsyncFormRequestMeta, AsyncFormAnswerMeta, AsyncFormSpec, FormAction } from "../../types/form";

export type VirtualItemData =
  | {
      type: "message";
      entry: TranscriptEntry;
      showSeparator: boolean;
      dateStr: string;
      groupWithPrevious: boolean;
      coalescedSegments?: CoalescedSegment[];
      /** True for the first message past a branch thread's fork point —
       *  renders a one-time "Forked here" divider ahead of it. See the
       *  `historyFloorTs` param on `buildMessageItems`. */
      showForkDivider?: boolean;
      /** Ids of artifacts this bubble's turn(s) produced — union of every
       *  entry folded into this row. Sourced from `entry.metadata.artifact_ids`
       *  (the live-finalized path) and/or `idsByTurnId` (the persisted
       *  `ArtifactWrite` tool_result scan — see `extractArtifactWriteResults`).
       *  Rendered as one `ArtifactCardTile` per id, deduped by id. */
      artifactIds?: string[];
    }
  | { type: "streaming"; prefixSegments?: CoalescedSegment[] };

/** Matches the skill-load hidden-user-message format produced by the engine
 *  extractor (see `crates/ao-engine/src/agent_runner.rs`):
 *    success: `[skill "<name>" loaded]\n<body>`
 *    failure: `[skill "<name>" load failed: <err>]` */
const SKILL_LOAD_LOADED_RE = /^\[skill "([^"]+)" loaded\]/;
const SKILL_LOAD_FAILED_RE = /^\[skill "([^"]+)" load failed:/;

/** Tailwind classes for a `role: "system"` bubble, keyed off
 *  `entry.metadata?.severity` — the tone hint the backend's
 *  `AgentEventPayload::SystemMessage` carries for a handful of emitters
 *  (presently: agent-watch contract authoring). Anything other than the
 *  known `"success"`/`"error"` strings — including the `undefined` every
 *  other system message still sends — renders the original neutral pill. */
export function systemMessageToneClass(severity: unknown): string {
  if (severity === "success") return "text-[var(--success)] bg-[var(--success)]/10";
  if (severity === "error") return "text-[var(--error)] bg-[var(--error)]/10";
  return "text-[var(--text-tertiary)] bg-[var(--bg-tertiary)]";
}

export function parseSkillLoadInfo(entry: TranscriptEntry): { skillName: string; success: boolean } | null {
  const content = entry.content ?? "";
  const loaded = SKILL_LOAD_LOADED_RE.exec(content);
  if (loaded) return { skillName: loaded[1], success: true };
  const failed = SKILL_LOAD_FAILED_RE.exec(content);
  if (failed) return { skillName: failed[1], success: false };
  return null;
}

function sameAgentSender(a: TranscriptEntry, b: TranscriptEntry): boolean {
  if (!isAgentMessage(a) || !isAgentMessage(b)) return false;
  const aId = (a.role as { agent: string }).agent;
  const bId = (b.role as { agent: string }).agent;
  return aId === bId;
}

const NO_GROUP_EVENT_TYPES = new Set(["form_answer", "form_request", "form_dismissed", "form_withdrawn"]);

/** Two entries are "same sender" if both are user messages OR both are agent
 *  messages from the same agent id. System, schedule, and form entries never group. */
function isSameSender(a: TranscriptEntry, b: TranscriptEntry): boolean {
  if (NO_GROUP_EVENT_TYPES.has(a.event_type) || NO_GROUP_EVENT_TYPES.has(b.event_type)) return false;
  if (isUserMessage(a) && isUserMessage(b)) return true;
  if (isAgentMessage(a) && isAgentMessage(b)) {
    const aId = (a.role as { agent: string }).agent;
    const bId = (b.role as { agent: string }).agent;
    return aId === bId;
  }
  return false;
}

/** Transcript schema: `tool_use` / `tool_result` event_types carry tool-call
 *  metadata (tool_use_id, tool_name, input, output, is_error, turn_id) but
 *  have empty `content`, so `MessageBubble.hasVisibleContent` suppresses them.
 *  Treating them as "invisible" at the list layer keeps `prevVisible`
 *  continuity working across a tool turn, lets turn-id coalescing find the
 *  previous agent bubble without walking past dead entries, and avoids
 *  reserving virtualizer slots for null bubbles. Live tool-call chips during
 *  streaming still come from `activeToolCalls`. */
function isToolTurnEntry(entry: TranscriptEntry): boolean {
  return entry.event_type === "tool_use" || entry.event_type === "tool_result";
}

/** True if the entry should be skipped over when building the visible flow.
 *  Combines the persisted `hidden_from_user` flag (skill-body injections,
 *  etc.) with empty-content tool-turn entries. */
function isSuppressedFromVisible(entry: TranscriptEntry): boolean {
  return !!entry.hidden_from_user || isToolTurnEntry(entry);
}

/** Map a raw tool-input question (JSON schema uses `type`) to a `FormFieldDef` (uses `kind`). */
function toolInputToFormField(q: Record<string, unknown>): FormFieldDef {
  return {
    id: q.id as string,
    kind: q.type as FormFieldDef["kind"],
    label: q.label as string,
    description: q.description as string | undefined,
    required: (q.required as boolean) ?? false,
    options: q.options as FormOptionDef[] | undefined,
    placeholder: q.placeholder as string | undefined,
    max_files: q.max_files as number | undefined,
    accept: q.accept as string | undefined,
  };
}

/**
 * Convert persisted AskUserQuestionWithForm tool_use/tool_result pairs into
 * synthetic form_answer entries so the reload case renders the same
 * FormAnswerBubble as the live-submit case.
 */
function preprocessFormToolPairs(messages: TranscriptEntry[]): TranscriptEntry[] {
  // Index tool_result entries by tool_use_id for fast lookup
  const resultByUseId = new Map<string, TranscriptEntry>();
  for (const entry of messages) {
    if (entry.event_type !== "tool_result") continue;
    const md = entry.metadata as Record<string, unknown> | null | undefined;
    const useId = md?.tool_use_id as string | undefined;
    if (useId) resultByUseId.set(useId, entry);
  }

  const skipSet = new Set<TranscriptEntry>();
  const result: TranscriptEntry[] = [];

  for (const entry of messages) {
    if (skipSet.has(entry)) { continue; }
    if (entry.event_type !== "tool_use") { result.push(entry); continue; }

    const md = entry.metadata as Record<string, unknown> | null | undefined;
    if (md?.tool_name !== "AskUserQuestionWithForm") { result.push(entry); continue; }

    // Found an AskUserQuestionWithForm tool_use — pair with its tool_result
    const useId = md.tool_use_id as string | undefined;
    const toolResult = useId ? resultByUseId.get(useId) : undefined;
    if (toolResult) skipSet.add(toolResult);

    const input = md.input as Record<string, unknown> | undefined;
    const title = (input?.title as string) ?? "";
    const intro = (input?.intro as string | undefined) ?? undefined;
    const rawQuestions = (input?.questions as Record<string, unknown>[] | undefined) ?? [];
    const fields: FormFieldDef[] = rawQuestions.map(toolInputToFormField);

    // Parse answers (or an action-button click) from the tool_result content (JSON)
    let answers: FormAnswerMap = {};
    let action: FormAction | undefined;
    let note: string | undefined;
    if (toolResult) {
      const raw = toolResult.content ?? (toolResult.metadata as Record<string, unknown> | null | undefined)?.output;
      try {
        const parsed = JSON.parse(typeof raw === "string" ? raw : JSON.stringify(raw ?? "{}"));
        answers = (parsed?.answers as FormAnswerMap) ?? {};
        action = parsed?.action as FormAction | undefined;
        note = parsed?.note as string | undefined;
      } catch { /* ignore parse errors */ }
    }

    const syntheticForm: FormRequestPayload = {
      form_id: useId ?? entry.ts,
      agent_id: "",
      session_id: "",
      title,
      intro,
      fields,
    };

    result.push({
      ts: entry.ts,
      role: "user",
      content: "",
      event_type: "form_answer",
      metadata: { form: syntheticForm, answers, action, note },
    });
  }

  return result;
}

/** Read the optional `metadata.turn_id` field. Returns `undefined`
 *  for legacy entries with no metadata or no turn_id. */
function getTurnId(entry: TranscriptEntry): string | undefined {
  const md = entry.metadata as Record<string, unknown> | null | undefined;
  const tid = md?.turn_id;
  return typeof tid === "string" ? tid : undefined;
}

/** Reads the client-only `metadata.artifact_ids` field `finalizeInFlightText`
 *  stamps onto a just-finalized live reply (see chatStore) — the snapshot of
 *  whatever `appendInFlightArtifactId` collected while that turn streamed.
 *  A fresh transcript fetch never carries this field; on reload the same ids
 *  come back through `extractArtifactWriteResults`'s tool_result scan below. */
function entryArtifactIds(entry: TranscriptEntry): string[] {
  const md = entry.metadata as Record<string, unknown> | null | undefined;
  const ids = md?.artifact_ids;
  return Array.isArray(ids) ? ids.filter((x): x is string => typeof x === "string") : [];
}

/** Unions two artifact-id lists, deduped, returning `undefined` when the
 *  result would be empty (keeps `VirtualItemData.artifactIds` optional). */
function mergeArtifactIds(existing: string[] | undefined, extra: string[]): string[] | undefined {
  if (extra.length === 0) return existing;
  const set = new Set(existing ?? []);
  for (const id of extra) set.add(id);
  return Array.from(set);
}

export interface ArtifactWriteExtraction {
  /** `turn_id` -> artifact ids produced by `ArtifactWrite` calls in that turn. */
  idsByTurnId: Map<string, string[]>;
  /** Card stubs (title/kind/refresh_intent) for every id found, to register
   *  into `useArtifactStore` so `ArtifactCardTile` can resolve them without
   *  a network round trip. */
  stubs: ArtifactCardStub[];
}

/** Scans a transcript for `ArtifactWrite` tool_use/tool_result pairs and
 *  extracts the artifact each one produced — the reload/historical half of
 *  inline card rendering. Mirrors `preprocessFormToolPairs`'s tool_use ->
 *  tool_result-by-`tool_use_id` correlation above; `tool_result` entries
 *  don't carry `tool_name` themselves (see `queue_tool_result_entry` on the
 *  backend), so the pairing has to go through the `tool_use` entry first.
 *  Pure — safe to unit test directly and to call from a `useMemo`. */
export function extractArtifactWriteResults(messages: TranscriptEntry[]): ArtifactWriteExtraction {
  const resultByUseId = new Map<string, TranscriptEntry>();
  for (const entry of messages) {
    if (entry.event_type !== "tool_result") continue;
    const md = entry.metadata as Record<string, unknown> | null | undefined;
    const useId = md?.tool_use_id;
    if (typeof useId === "string") resultByUseId.set(useId, entry);
  }

  const idsByTurnId = new Map<string, string[]>();
  const stubs: ArtifactCardStub[] = [];
  for (const entry of messages) {
    if (entry.event_type !== "tool_use") continue;
    const md = entry.metadata as Record<string, unknown> | null | undefined;
    // `tool_name` is persisted MCP-qualified for CLI-mode agents (e.g.
    // `mcp__launchpad__ArtifactWrite`) — strip the transport prefix before
    // comparing, same as the live SSE path in `useSSE.ts`.
    if (typeof md?.tool_name !== "string" || stripMcpPrefix(md.tool_name) !== "ArtifactWrite") continue;
    const useId = md?.tool_use_id;
    const turnId = md?.turn_id;
    if (typeof useId !== "string" || typeof turnId !== "string") continue;

    const result = resultByUseId.get(useId);
    const resultMd = result?.metadata as Record<string, unknown> | null | undefined;
    const stub = parseArtifactWriteOutput(resultMd?.output);
    if (!stub) continue;

    stubs.push(stub);
    const list = idsByTurnId.get(turnId) ?? [];
    list.push(stub.id);
    idsByTurnId.set(turnId, list);
  }
  return { idsByTurnId, stubs };
}

/** Result returned by `buildMessageItems` — items list plus the unattached
 *  skill-load chips that belong to the in-flight streaming turn (rendered as
 *  prefix segments on the streaming sentinel by the caller). */
export interface BuildMessageItemsResult {
  items: (VirtualItemData & { type: "message" })[];
  orphanChips: { skillName: string; success: boolean }[];
}

/** Build the non-streaming portion of the virtualized list from a transcript.
 *
 * Pure function — extracted out of the `MessageList` `useMemo` body so it can
 * be unit-tested. The caller appends the streaming sentinel (with optional
 * `prefixSegments` derived from `orphanChips`) after this returns.
 *
 * Coalesce passes, in order:
 *   1. **Skill-load chips into prev bubble.** Hidden skill-load entries
 *      become inline chips attached to the next visible message; when prev
 *      is an agent bubble from the same agent the chips fold INTO prev's
 *      bubble so a skill load reads as one continuous thought.
 *   2. **Turn-id coalesce.** Consecutive agent-role entries sharing
 *      `metadata.turn_id` (the continuation runner can respawn mid-turn,
 *      emitting multiple `response` events under one `turn_id`) fold into a
 *      single bubble. Without this the user sees
 *      e.g. a speculative-failure bubble followed by a real-success bubble
 *      from one model turn.
 *   3. **Chip-prepend.** Chips attached to an agent message where prev
 *      isn't the same sender — render the chips at the top of the new
 *      bubble.
 *   4. **Plain push.** No coalesce applies.
 *
 * `historyFloorTs` (a branch thread's fork-point ISO timestamp, or
 * `null`/`undefined` for a non-branch thread) marks exactly one item —
 * the first visible message strictly after the floor whose predecessor (or
 * absence of one) is at/before it — with `showForkDivider: true`, so the
 * caller can render a one-time "Forked here" divider ahead of it. This
 * naturally lands wherever the inherited/own boundary currently sits in the
 * loaded window, however that window was assembled (initial tail-merge or a
 * "load older" page crossing the boundary), and never re-fires for messages
 * purely on one side of it. */
const EMPTY_IDS_BY_TURN_ID: Map<string, string[]> = new Map();

export function buildMessageItems(
  messages: TranscriptEntry[],
  historyFloorTs?: string | null,
  idsByTurnId: Map<string, string[]> = EMPTY_IDS_BY_TURN_ID
): BuildMessageItemsResult {
  const floorMs = historyFloorTs ? new Date(historyFloorTs).getTime() : null;
  // Union of an entry's own client-stamped ids (live-finalized replies) and
  // whatever `idsByTurnId` resolved for its turn (persisted tool_result
  // scan) — see `entryArtifactIds` / `extractArtifactWriteResults`.
  const idsForEntry = (entry: TranscriptEntry): string[] => {
    const turnId = getTurnId(entry);
    const persisted = turnId ? idsByTurnId.get(turnId) ?? [] : [];
    const live = entryArtifactIds(entry);
    return persisted.length === 0 ? live : live.length === 0 ? persisted : [...persisted, ...live];
  };
  // Pass 1: attach skill-load chips to the next non-hidden, non-tool-turn
  // visible message index. Hidden skill-loads with no following visible
  // message are "orphan chips" — they belong to the currently-streaming
  // agent turn and get attached to the streaming sentinel by the caller.
  const chipsByNextOrigIdx = new Map<number, { skillName: string; success: boolean }[]>();
  const orphanChips: { skillName: string; success: boolean }[] = [];
  for (let k = 0; k < messages.length; k++) {
    const m = messages[k];
    if (!m.hidden_from_user) continue;
    const info = parseSkillLoadInfo(m);
    if (!info) continue;
    let next = k + 1;
    while (next < messages.length && isSuppressedFromVisible(messages[next])) next++;
    if (next >= messages.length) {
      orphanChips.push(info);
      continue;
    }
    const list = chipsByNextOrigIdx.get(next) ?? [];
    list.push(info);
    chipsByNextOrigIdx.set(next, list);
  }

  const visible = messages
    .map((entry, origIdx) => ({ entry, origIdx }))
    .filter((x) => !isSuppressedFromVisible(x.entry));

  const items: (VirtualItemData & { type: "message" })[] = [];
  for (let i = 0; i < visible.length; i++) {
    const { entry, origIdx } = visible[i];
    const chips = chipsByNextOrigIdx.get(origIdx) ?? [];
    const curDate = utcToLocalDate(entry.ts);
    const prevVisible = i > 0 ? visible[i - 1].entry : null;
    const prevDate = prevVisible ? utcToLocalDate(prevVisible.ts) : null;
    const showSeparator = curDate !== prevDate;
    const groupWithPrevious =
      i > 0 && !showSeparator && prevVisible != null && isSameSender(prevVisible, entry);
    // See the `historyFloorTs` doc comment above — true exactly once, on the
    // first visible entry whose predecessor (or absence of one) sits at/
    // before the fork point and whose own ts is strictly after it.
    const showForkDivider =
      floorMs != null &&
      new Date(entry.ts).getTime() > floorMs &&
      (prevVisible == null || new Date(prevVisible.ts).getTime() <= floorMs);

    const prevItem = items.length > 0 ? items[items.length - 1] : null;

    // Pass 2a — chip-coalesce: previous bubble is an agent message from the
    // same agent AND one or more skill-load chips sit between them. Merge
    // into the previous item's segments so everything renders as one bubble.
    //
    // We don't require `prevItem.entry === prevVisible`: after a chain like
    // text→chip→text→chip→text, the second iteration's coalesce leaves
    // `prevItem.entry` pointing at the first absorbed entry, not the second.
    // The loop invariant — every iteration either pushes or folds, so
    // items.top always represents visible[i-1] — keeps this safe.
    const canCoalesceIntoPrev =
      chips.length > 0 && prevVisible != null && sameAgentSender(prevVisible, entry) && prevItem != null;

    if (canCoalesceIntoPrev && prevItem) {
      let segments: CoalescedSegment[] = prevItem.coalescedSegments ?? [];
      if (segments.length === 0) {
        const prevText = prevVisible!.content ?? "";
        if (prevText.trim().length > 0) segments = [{ kind: "text", content: prevText }];
      }
      for (const info of chips) {
        segments = [...segments, { kind: "chip", skillName: info.skillName, success: info.success }];
      }
      const curText = entry.content ?? "";
      if (curText.trim().length > 0) {
        segments = [...segments, { kind: "text", content: curText }];
      }
      prevItem.coalescedSegments = segments;
      prevItem.artifactIds = mergeArtifactIds(prevItem.artifactIds, idsForEntry(entry));
      // `entry` folded into prevItem's row rather than getting its own —
      // reattach the divider to that row so the transition isn't silently
      // dropped (see the `historyFloorTs` doc comment above).
      if (showForkDivider) prevItem.showForkDivider = true;
      continue;
    }

    // Pass 2b — turn-id coalesce: no chips bridging, but this entry is an
    // agent message from the same agent as the previous bubble AND shares
    // `metadata.turn_id` with it. The continuation runner can fire
    // multiple `response` events under a single logical turn (e.g. model
    // emits XML + speculative prose → dispatch → respawn delivers real
    // outcome prose) — fold them into one bubble so the user sees one
    // self-correcting unit instead of two separate (sometimes
    // contradictory) bubbles.
    const curTurnId = getTurnId(entry);
    const canCoalesceByTurnId =
      chips.length === 0 &&
      curTurnId != null &&
      prevItem != null &&
      isAgentMessage(entry) &&
      isAgentMessage(prevItem.entry) &&
      sameAgentSender(prevItem.entry, entry) &&
      getTurnId(prevItem.entry) === curTurnId;

    if (canCoalesceByTurnId && prevItem) {
      let segments: CoalescedSegment[] = prevItem.coalescedSegments ?? [];
      if (segments.length === 0) {
        const prevText = prevItem.entry.content ?? "";
        if (prevText.trim().length > 0) segments = [{ kind: "text", content: prevText }];
      }
      const curText = entry.content ?? "";
      if (curText.trim().length > 0) {
        segments = [...segments, { kind: "text", content: curText }];
      }
      // Even if both chunks were whitespace-only, leave the bubble in
      // place — MessageBubble's `hasVisibleContent` will suppress empties.
      if (segments.length > 0) {
        prevItem.coalescedSegments = segments;
      }
      prevItem.artifactIds = mergeArtifactIds(prevItem.artifactIds, idsForEntry(entry));
      if (showForkDivider) prevItem.showForkDivider = true;
      continue;
    }

    // Pass 3 — chip-prepend: chips attach to an agent message but prev
    // isn't the same sender (or there is no prev). Prepend chips as
    // segments so the user still sees what skill was loaded even when the
    // preceding assistant message was suppressed (e.g. the agent emitted
    // only `<load_skill />` with no surrounding text).
    if (chips.length > 0 && isAgentMessage(entry)) {
      const segments: CoalescedSegment[] = chips.map((info) => ({
        kind: "chip" as const,
        skillName: info.skillName,
        success: info.success,
      }));
      const curText = entry.content ?? "";
      if (curText.trim().length > 0) {
        segments.push({ kind: "text", content: curText });
      }
      items.push({
        type: "message",
        entry,
        showSeparator,
        dateStr: curDate,
        groupWithPrevious,
        coalescedSegments: segments,
        showForkDivider,
        artifactIds: mergeArtifactIds(undefined, idsForEntry(entry)),
      });
      continue;
    }

    // Pass 4 — plain push.
    items.push({
      type: "message",
      entry,
      showSeparator,
      dateStr: curDate,
      groupWithPrevious,
      showForkDivider,
      artifactIds: mergeArtifactIds(undefined, idsForEntry(entry)),
    });
  }

  return { items, orphanChips };
}

function estimateMessageHeight(item: VirtualItemData): number {
  if (item.type === "streaming") {
    let extra = 0;
    if (item.prefixSegments) {
      const textLen = item.prefixSegments.reduce(
        (acc, seg) => (seg.kind === "text" ? acc + seg.content.length : acc),
        0
      );
      const chipCount = item.prefixSegments.filter((s) => s.kind === "chip").length;
      extra = Math.min(textLen * 0.6, 200) + chipCount * 28;
    }
    return 120 + extra;
  }
  // Height estimate only — real layout takes over via `measureElement`, so
  // approximating the fork divider's ~36px alongside the date separator's
  // ~44px is enough to avoid an initial scroll-jump.
  const separatorHeight = (item.showSeparator ? 44 : 0) + (item.showForkDivider ? 36 : 0);
  if (item.entry.event_type === "form_request") {
    const md = item.entry.metadata as AsyncFormRequestMeta | null | undefined;
    const fieldCount = md?.spec?.fields?.length ?? 0;
    return separatorHeight + 120 + fieldCount * 72;
  }
  if (item.entry.event_type === "form_answer") {
    const md = item.entry.metadata as Record<string, unknown> | null | undefined;
    const form = md?.form as FormRequestPayload | undefined;
    // Async answers carry their field count under `spec` (see
    // `AsyncFormAnswerMeta.spec`), sync answers under `form` — either one
    // renders the full interactive-form-sized bubble; neither present falls
    // back to the short values-list bubble's estimate.
    const spec = md?.spec as AsyncFormSpec | undefined;
    const fieldCount = form?.fields?.length ?? spec?.fields?.length ?? 0;
    return separatorHeight + (form || spec ? 120 + fieldCount * 72 : 80);
  }
  if (item.entry.event_type === "form_dismissed") return separatorHeight + 44;
  if (item.entry.event_type === "form_withdrawn") return separatorHeight + 44;
  // Grouped agent continuations drop the name label (~22px)
  const groupedAdjust = item.groupWithPrevious ? -22 : 0;
  // Coalesced bubbles roll several segments into one; sum text lengths and
  // add a flat ~28px per inline chip for a rough estimate.
  let textLen: number;
  let chipAdjust = 0;
  if (item.coalescedSegments && item.coalescedSegments.length > 0) {
    textLen = item.coalescedSegments.reduce(
      (acc, seg) => (seg.kind === "text" ? acc + seg.content.length : acc),
      0
    );
    chipAdjust = item.coalescedSegments.filter((s) => s.kind === "chip").length * 28;
  } else {
    textLen = item.entry.content?.length ?? 0;
  }
  if (textLen < 100) return separatorHeight + 72 + groupedAdjust + chipAdjust;
  if (textLen < 500) return separatorHeight + 160 + groupedAdjust + chipAdjust;
  return separatorHeight + 280 + groupedAdjust + chipAdjust;
}

/** Defensive cap on consecutive auto-triggered older-page fetches while the
 *  container isn't scrollable (see `shouldLoadOlderMessages` below). Real,
 *  user-driven scrolling never counts against this — it only backstops a
 *  runaway loop if `hasMoreMessages` were ever wrong about history being
 *  exhausted. */
export const MAX_AUTO_BACKFILL_ITERATIONS = 200;

export interface LoadOlderCheckParams {
  hasMoreMessages: boolean;
  loadingMore: boolean;
  loadMoreInFlight: boolean;
  loadMoreCooldown: boolean;
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
  nearTopByIndex: boolean;
  autoBackfillCount: number;
}

export interface LoadOlderCheckResult {
  shouldFetch: boolean;
  nextAutoBackfillCount: number;
}

/** Pure decision at the heart of `maybeLoadOlder`: should the older-page fetch
 *  fire right now? Factored out so the not-scrollable backfill case (a short
 *  thread, or one whose recent replies are collapsed behind "Read more" so
 *  their rendered height is small — never overflows the viewport, so no
 *  `scroll` event can ever fire) can be pinned with a direct unit test rather
 *  than a full DOM/virtualizer render. */
export function shouldLoadOlderMessages(params: LoadOlderCheckParams): LoadOlderCheckResult {
  const {
    hasMoreMessages,
    loadingMore,
    loadMoreInFlight,
    loadMoreCooldown,
    scrollTop,
    scrollHeight,
    clientHeight,
    nearTopByIndex,
    autoBackfillCount,
  } = params;

  if (!hasMoreMessages || loadingMore || loadMoreInFlight || loadMoreCooldown) {
    return { shouldFetch: false, nextAutoBackfillCount: autoBackfillCount };
  }

  const notScrollable = scrollHeight - clientHeight <= 4;
  if (notScrollable) {
    if (autoBackfillCount >= MAX_AUTO_BACKFILL_ITERATIONS) {
      return { shouldFetch: false, nextAutoBackfillCount: autoBackfillCount };
    }
    return { shouldFetch: true, nextAutoBackfillCount: autoBackfillCount + 1 };
  }

  if (scrollTop >= 50 && !nearTopByIndex) {
    return { shouldFetch: false, nextAutoBackfillCount: 0 };
  }
  return { shouldFetch: true, nextAutoBackfillCount: 0 };
}

/** Gate for the top-of-history `IntersectionObserver` (see `MessageList`):
 *  consult the older-page loader only when the sentinel is actually
 *  intersecting AND the initial bottom-scroll/restore has settled
 *  (`observerReady`).
 *
 *  The readiness gate exists because on mount (and on thread switch) the list
 *  paints once before `scrollToBottom` pins the viewport; in that pre-scroll
 *  frame the top sentinel can momentarily sit in view, which would otherwise
 *  page in history the instant the thread opens. The real fetch decision —
 *  scrollability, near-top, and the in-flight / cooldown / backfill-cap guards
 *  — still runs inside `shouldLoadOlderMessages`; this only decides whether to
 *  even call `maybeLoadOlder`. Factored out so the observer's gating is
 *  unit-testable without a real DOM `IntersectionObserver` (jsdom has none). */
export function shouldObserverTriggerLoad(params: {
  isIntersecting: boolean;
  observerReady: boolean;
}): boolean {
  return params.isIntersecting && params.observerReady;
}

/** Resolves the `channelFallbackFormId` input to the inline placeholder's
 *  `pendingFormId` derivation (see `MessageList`'s own `pendingFormId`
 *  computation) — `pendingAsyncFormIdByChannel`'s entry for the channel
 *  currently on screen, gated to genuine project channels (`selectedAgentId`
 *  = `project:{id}`) only.
 *
 *  `pendingAsyncFormIdByChannel` is a per-channel-key map (see its docstring
 *  in chatStore.ts) that today is only ever written with `project:{id}` keys
 *  (`projectStore.ts`, `useProjectSSE.ts`). Reading it by raw `selectedAgentId`
 *  with no gate meant an agent's personal chat would just as happily pick up
 *  a stray entry as a project channel would — nothing enforced that the
 *  lookup key was actually a project channel, so the fallback could resolve
 *  a form id for a channel/agent it was never posted to and surface it in
 *  the wrong place. Since the project fallback exists specifically to cover
 *  project channels' lack of a thread concept, restricting it to channels
 *  that are actually `project:{id}` makes that invariant explicit instead of
 *  relying on every future writer of `pendingAsyncFormIdByChannel` to
 *  independently maintain it. */
export function resolveChannelFallbackFormId(
  selectedAgentId: string | null,
  pendingAsyncFormIdByChannel: Record<string, string | null>,
): string | null {
  if (!selectedAgentId?.startsWith("project:")) return null;
  return pendingAsyncFormIdByChannel[selectedAgentId] ?? null;
}

/** The four ways a `form_request` transcript entry's inline bubble can
 *  render, in priority order. Every entry reaching this decision is
 *  necessarily an *async* form: the sync path (`AskUserQuestionWithForm`)
 *  always writes its own `form_request` entry with `hidden_from_user: true`
 *  (see `types/form.ts`'s `AsyncFormRequestMeta` doc), which
 *  `isSuppressedFromVisible` filters out of `buildMessageItems`'s `visible`
 *  list before any entry reaches this component's render loop — so "is it
 *  sync" never needs checking here at all.
 *
 *  Replaces the old single `isPending` boolean, which used "not pending" as
 *  an implicit stand-in for "answered" — true only while nothing else had
 *  happened in the thread yet. The moment `isPendingFormStillCurrent` (see
 *  `chatStore.ts`) flipped false — the near-universal case for an async
 *  form, since the agent narrates right after posting one — a genuinely
 *  unanswered, undismissed form fell through to the read-only branch with no
 *  `initialAnswers`: a blank, permanently unanswerable bubble. The backend's
 *  `async_form_answer` route never actually stopped accepting a late answer
 *  (it only checks plain `pending_forms` membership, not latestness), so
 *  "interactive" is the correct fallback once "pending", "dismissed", and
 *  "answered" are all ruled out. */
export function resolveFormRequestBranch(params: {
  isPending: boolean;
  isDismissed: boolean;
  isAnswered: boolean;
}): "pending" | "dismissed" | "answered" | "interactive" {
  if (params.isPending) return "pending";
  if (params.isDismissed) return "dismissed";
  if (params.isAnswered) return "answered";
  return "interactive";
}

/** Converts an answered async `form_answer` entry's own spec snapshot (see
 *  the backend's `FormAnswerMeta.spec` —
 *  crates/ao-engine-tools-core/src/form_events.rs) into the
 *  `FormRequestPayload` shape `AskUserQuestionForm` renders from — the same
 *  conversion `AsyncFormRequestCard` does for a still-pending form's spec,
 *  just for an already-answered one. Split out from the JSX closure so the
 *  conversion — not just the shared form widget it feeds — has a direct
 *  unit test, same as `resolveFormRequestBranch`/`resolveInlineAsyncFormTarget`
 *  above.
 *
 *  Returns `null` when `spec` is absent — a legacy entry persisted before
 *  the spec snapshot existed — so the caller falls back to the plain
 *  values-list bubble (`AsyncFormAnswerBubble`) instead of a form with no
 *  fields to render. */
export function buildAnsweredAsyncForm(
  formId: unknown,
  spec: AsyncFormSpec | undefined,
): FormRequestPayload | null {
  if (!spec) return null;
  return {
    form_id: typeof formId === "string" ? formId : spec.form_id,
    agent_id: "",
    session_id: "",
    title: spec.title,
    intro: spec.intro ?? undefined,
    fields: spec.fields,
  };
}

/** Which async-form-answer/dismiss REST call the "interactive" branch above
 *  makes for the channel currently on screen — the same two shapes the
 *  floating overlay uses (`submitAsyncFormAnswer`/`dismissAsyncForm` for an
 *  agent channel in ChatView.tsx, the `*ForProject` variants for a
 *  `project:{id}` channel in ProjectWorkspace.tsx). Split out from the JSX
 *  closure so the routing decision itself — not just the generic form
 *  widget it feeds — has a direct unit test. */
export function resolveInlineAsyncFormTarget(
  selectedAgentId: string | null,
): { kind: "agent"; agentId: string } | { kind: "project"; projectId: string } | { kind: "none" } {
  if (!selectedAgentId) return { kind: "none" };
  if (selectedAgentId.startsWith("project:")) {
    return { kind: "project", projectId: selectedAgentId.slice("project:".length) };
  }
  return { kind: "agent", agentId: selectedAgentId };
}

export function MessageList() {
  const messages = useChatStore((s) => s.messages);
  const hasMoreMessages = useChatStore((s) => s.hasMoreMessages);
  const messagesLoading = useChatStore((s) => s.messagesLoading);
  const loadingMore = useChatStore((s) => s.loadingMore);
  const loadMoreMessages = useChatStore((s) => s.loadMoreMessages);
  const selectedAgentId = useChatStore((s) => s.selectedAgentId);
  // The thread currently shown in the conversation column — in-flight state
  // (typing bubble / text buffer / tool chips / thinking pill) is scoped to
  // it so a run on another thread never renders here. `undefined` for the
  // default thread, matching the backend's event tagging.
  const streamingThreadId = useStreamingThreadId(selectedAgentId);
  const streamingText = useStreamingText(selectedAgentId, streamingThreadId);
  const activeToolCalls = useActiveToolCalls(selectedAgentId, streamingThreadId);
  const inFlightThinking = useInFlightThinking(selectedAgentId, streamingThreadId);
  // Stable boolean — true while the in-flight entry exists (covers active
  // streaming, tool calls, typing, AND the 400 ms debounce window between
  // run_ended and the next run_started). Using entry existence rather than
  // content presence keeps orphan skill-load chips alive as prefixSegments
  // across the turn handoff, preventing the bubble from unmounting mid-loop.
  const hasInFlightBubble = useChatStore((s) =>
    s.selectedAgentId ? s.inFlightByAgent.has(inFlightKey(s.selectedAgentId, streamingThreadId)) : false
  );
  const setStreamingAreaVisible = useChatStore((s) => s.setStreamingAreaVisible);
  const agents = useChatStore((s) => s.agents);
  // For agent channels: pending async form id comes from the agent snapshot,
  // scoped to whichever thread is actually on screen (streamingThreadId,
  // computed above) — a form pending on a different thread of this agent
  // must not render here. For project channels (selectedAgentId =
  // `project:{id}`): comes from pendingAsyncFormIdByChannel, set by
  // useProjectSSE on form_posted (projects have no thread concept) — gated
  // to genuine project channels by resolveChannelFallbackFormId so it can
  // never surface for an agent's personal chat.
  const pendingAsyncFormIdByChannel = useChatStore((s) => s.pendingAsyncFormIdByChannel);
  // Same actions the floating `AsyncFormRequestCard` overlay uses to retire a
  // form's pending-pointer once answered/dismissed (ChatView.tsx / ProjectWorkspace.tsx)
  // — the inline "stale but still answerable" form below wires to these too,
  // guarded to only clear the channel-wide project pointer when it still
  // names *this* form_id (a stale form is by definition not the one the
  // pointer currently tracks; the agent-scoped variant filters by form_id
  // itself so it needs no such guard).
  const clearPendingAsyncForm = useChatStore((s) => s.clearPendingAsyncForm);
  const clearPendingAsyncFormId = useChatStore((s) => s.clearPendingAsyncFormId);
  // No staleness check against `messages` here anymore — see
  // `isPendingFormStillCurrent`'s docstring for why presence in
  // `pending_forms` is now the whole answer to "is this still pending."
  const pendingFormId =
    pendingFormForThread(
      agents.find((a) => a.agent_id === selectedAgentId)?.pending_forms,
      streamingThreadId
    )?.form_id ?? resolveChannelFallbackFormId(selectedAgentId, pendingAsyncFormIdByChannel);

  // Correlate async form lifecycle entries by form_id: the answer carries the
  // submitted values (and, since the spec-snapshot fix, its own self-
  // contained spec — see the `form_answer` render branch below), a
  // dismissal marks it declined. Unlike the answer, an unanswered/dismissed
  // `form_request` entry's own spec has no snapshot elsewhere to fall back
  // on, so its still-interactive/dismissed rendering (below) does need this
  // per-form_id join — it just never needs a *spec* lookup here anymore.
  const { answersByFormId, dismissedFormIds } = useMemo(() => {
    const answers = new Map<string, FormAnswerMap>();
    const dismissed = new Set<string>();
    for (const m of messages) {
      const md = m.metadata as Record<string, unknown> | null | undefined;
      if (m.event_type === "form_answer") {
        const fid = md?.form_id;
        const vals = md?.values;
        if (typeof fid === "string" && vals && typeof vals === "object") {
          answers.set(fid, vals as FormAnswerMap);
        }
      } else if (m.event_type === "form_dismissed") {
        const fid = md?.form_id;
        if (typeof fid === "string") dismissed.add(fid);
      }
    }
    return { answersByFormId: answers, dismissedFormIds: dismissed };
  }, [messages]);

  const scrollToBottomTrigger = useChatStore((s) => s.scrollToBottomTrigger);

  const containerRef = useRef<HTMLDivElement>(null);
  // Zero-ish-height marker pinned at content-offset 0 (the true top of all
  // loaded history). An IntersectionObserver watches it to fire older-page
  // loads on visibility rather than on scroll events — see the observer effect
  // below and `shouldObserverTriggerLoad`.
  const topSentinelRef = useRef<HTMLDivElement>(null);
  // Armed only once the initial/thread-switch bottom-scroll has settled, so the
  // observer can't page in history during the pre-scroll frame.
  const observerReadyRef = useRef(false);
  const wasNearBottomRef = useRef(true);
  const lastScrollTopRef = useRef(0);
  const prevMessagesLenRef = useRef(messages.length);
  const loadMoreAnchorIndexRef = useRef<number | null>(null);
  const loadMoreInFlightRef = useRef(false);
  const loadMoreCooldownRef = useRef(false);
  const scrollTimeoutRef = useRef<ReturnType<typeof setTimeout>>(null);
  const stickyRafRef = useRef<number | null>(null);
  const [stickyDate, setStickyDate] = useState<string | null>(null);
  const [stickyVisible, setStickyVisible] = useState(false);
  const [showScrollToBottom, setShowScrollToBottom] = useState(false);

  const targetMessageTs = useBookmarkStore((s) => s.targetMessageTs);
  const clearScrollTarget = useBookmarkStore((s) => s.clearScrollTarget);

  const resolved = useResolvedAgent(selectedAgentId ?? "");
  const agentName = resolved.displayName || "Agent";
  const agentEmoji = resolved.emoji;

  // Branching is meaningful only for a concrete agent's chat. Project channels
  // (`project:{id}`) have no per-agent thread dimension, so suppress the
  // affordance there.
  const allowBranch = !!selectedAgentId && !selectedAgentId.startsWith("project:");

  // The currently-viewed thread's fork point, when it's a branch — drives
  // the "Forked here" divider in `buildMessageItems` below. `null` for
  // project channels, default threads, and fresh (non-branch) threads.
  const threadsForAgent = useChatStore((s) =>
    allowBranch && selectedAgentId ? s.threadsByAgent.get(selectedAgentId) : undefined
  );
  const selectedThreadIdForAgent = useChatStore((s) =>
    allowBranch && selectedAgentId ? s.selectedThreadIdByAgent.get(selectedAgentId) : undefined
  );
  const currentThread = threadsForAgent?.find(
    (t) => t.id === (selectedThreadIdForAgent ?? threadsForAgent.find((d) => d.kind === "default")?.id)
  );
  const historyFloorTs = currentThread?.branch_source ? currentThread.history_floor_ts ?? null : null;

  // Reload/historical half of inline artifact cards: scan the loaded
  // transcript for ArtifactWrite tool_use/tool_result pairs once per
  // `messages` change. `idsByTurnId` feeds `buildMessageItems` below;
  // `stubs` get registered into `useArtifactStore` (a side effect, so it
  // runs in the `useEffect` beneath, not here) so `ArtifactCardTile` can
  // resolve title/kind without a network round trip.
  const { idsByTurnId, stubs: artifactStubs } = useMemo(
    () => extractArtifactWriteResults(messages),
    [messages]
  );
  useEffect(() => {
    if (artifactStubs.length === 0) return;
    const registerCard = useArtifactStore.getState().registerCard;
    artifactStubs.forEach(registerCard);
  }, [artifactStubs]);

  // Build flat virtual item list.
  //
  // Pure pass (`buildMessageItems`) handles three coalesce shapes: skill-load
  // chips folded into prev bubble, turn-id continuation folded into prev
  // bubble, and chip-prepend onto a new bubble. After that the
  // streaming sentinel is appended with optional orphan-chip prefix.
  const virtualItems = useMemo((): VirtualItemData[] => {
    const { items: messageItems, orphanChips } = buildMessageItems(
      preprocessFormToolPairs(messages),
      historyFloorTs,
      idsByTurnId
    );
    const items: VirtualItemData[] = [...messageItems];

    // Streaming coalesce: attach orphan skill-load chips as a prefix on the
    // streaming sentinel so the user sees which skills were loaded as part of
    // the current turn.
    //
    // We intentionally leave the pre-skill text (the committed bubble that
    // landed just before the skill load) as its own separate entry rather than
    // absorbing it into the sentinel. Absorbing it makes the sentinel tall
    // enough that auto-scroll pins to the bottom, hiding the prefix text above
    // the viewport fold — the user sees only the chips and post-skill text, not
    // the opener. Keeping the pre-skill bubble separate means it stays visible
    // where it was; the sentinel shows only new content (chips + streaming
    // text). The static buildMessageItems pass coalesces everything into one
    // bubble once the run ends (Pass 2a handles chip-bridged same-agent pairs).
    let streamingPrefix: CoalescedSegment[] | undefined;
    if (hasInFlightBubble && orphanChips.length > 0) {
      streamingPrefix = orphanChips.map((info) => ({
        kind: "chip" as const,
        skillName: info.skillName,
        success: info.success,
      }));
    }

    // Always append streaming sentinel — StreamingMessage returns null internally if inactive
    items.push({ type: "streaming", prefixSegments: streamingPrefix });
    return items;
  }, [messages, hasInFlightBubble, selectedAgentId, historyFloorTs, idsByTurnId]);

  const virtualizer = useVirtualizer({
    count: virtualItems.length,
    getScrollElement: () => containerRef.current,
    estimateSize: (index) => estimateMessageHeight(virtualItems[index]),
    overscan: 15,
    // Start at the bottom from the very first render — the browser clamps
    // scrollTop to scrollHeight, so the virtualizer immediately renders the
    // last items. This prevents the flash of mid-conversation content before
    // scrollToBottom() runs.
    initialOffset: Number.MAX_SAFE_INTEGER,
  });


  /** Check if user is scrolled near the bottom of the container. */
  const isNearBottom = useCallback(() => {
    const el = containerRef.current;
    if (!el) return true;
    return el.scrollHeight - el.scrollTop - el.clientHeight < 100;
  }, []);

  /** Scroll the container to the very bottom.
   *
   * The virtualizer measures rendered rows asynchronously via ResizeObserver,
   * so `totalSize` (and thus `scrollHeight`) keeps shifting for a few frames
   * after the first scroll. Set once and we'd land correctly, then a later
   * measurement shrinks total and the browser clamps scrollTop up a few
   * pixels — reads as an "overshoot then snap back" glitch. Re-pin each frame
   * until the height stabilizes (capped so lazy-loading content can't loop). */
  const scrollToBottom = useCallback(() => {
    const el = containerRef.current;
    if (!el || virtualItems.length === 0) return;
    virtualizer.scrollToIndex(virtualItems.length - 1, { align: "end" });

    let prevHeight = -1;
    let stableFrames = 0;
    let framesRemaining = 12;
    const pin = () => {
      const c = containerRef.current;
      if (!c || framesRemaining-- <= 0) return;
      const h = c.scrollHeight;
      const target = Math.max(0, h - c.clientHeight);
      if (Math.abs(c.scrollTop - target) > 0.5) c.scrollTop = target;
      if (h === prevHeight) {
        if (++stableFrames >= 2) return;
      } else {
        stableFrames = 0;
        prevHeight = h;
      }
      requestAnimationFrame(pin);
    };
    requestAnimationFrame(pin);
  }, [virtualizer, virtualItems.length]);

  // Auto-scroll to bottom when new messages arrive (only if user was near bottom)
  // Skip during load-more — scroll preservation handles that case separately.
  useEffect(() => {
    const prevLen = prevMessagesLenRef.current;
    const curLen = messages.length;

    if (curLen > prevLen && wasNearBottomRef.current && !loadMoreInFlightRef.current) {
      requestAnimationFrame(scrollToBottom);
    }

    prevMessagesLenRef.current = curLen;
  }, [messages, scrollToBottom]);

  // Force scroll to bottom when user sends a message (regardless of scroll position)
  const prevScrollTriggerRef = useRef(scrollToBottomTrigger);
  useEffect(() => {
    if (scrollToBottomTrigger > prevScrollTriggerRef.current) {
      requestAnimationFrame(scrollToBottom);
    }
    prevScrollTriggerRef.current = scrollToBottomTrigger;
  }, [scrollToBottomTrigger, scrollToBottom]);

  // Scroll to bottom on initial load.
  // The component remounts on agent switch (keyed by agentId in parent),
  // so this only needs to handle the initial render.
  useLayoutEffect(() => {
    if (messagesLoading) {
      // A fresh transcript is loading (initial mount or thread switch). Disarm
      // the top-history observer until the post-load bottom-scroll settles, so
      // it can't page in older history while we're still positioning the
      // viewport at the bottom of the newly-loaded thread.
      observerReadyRef.current = false;
      return;
    }
    if (messages.length > 0) {
      scrollToBottom();
      // Arm the observer only after this frame's bottom-scroll is applied — the
      // initial paint can briefly leave the top sentinel in view before
      // scrollToBottom pins the viewport.
      requestAnimationFrame(() => {
        observerReadyRef.current = true;
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [messagesLoading]);

  // Auto-scroll as streaming text grows, tool calls change, or the thinking
  // pill opens/grows (only if user was near bottom).
  //
  // Thinking inclusion: when `thinking_started` arrives the streaming bubble
  // mounts with a "Thinking…" pill, then `thinking_delta` events grow its
  // preview line. Without subscribing to thinking state here, the bubble
  // could grow downward past the fold while we're still pinned to the old
  // scrollHeight — the user has to scroll manually to see the pill. The
  // memoized `inFlightThinking` ref only changes on `active` flip, buffer
  // growth, or `elapsedMs` set, so this stays quiet on idle re-renders.
  useEffect(() => {
    if (
      (streamingText || activeToolCalls.length > 0 || inFlightThinking) &&
      wasNearBottomRef.current
    ) {
      requestAnimationFrame(scrollToBottom);
    }
  }, [streamingText, activeToolCalls, inFlightThinking, scrollToBottom]);

  // Track whether the streaming bubble is on-screen via scroll position. The
  // bubble is always the last item, so "near bottom" is equivalent to "bubble
  // in view". Using scroll position instead of IntersectionObserver avoids a
  // mount-time race where the observer's initial callback fired false before
  // scrollToBottom() settled, flashing the floating pill on navigation.
  const isStreaming = hasInFlightBubble;

  useEffect(() => {
    if (!isStreaming) {
      setStreamingAreaVisible(true);
    }
  }, [isStreaming, setStreamingAreaVisible]);

  // Scroll to a bookmarked message when targetMessageTs is set
  const targetBookmark = useBookmarkStore((s) => s.targetBookmark);
  const setPinnedBookmark = useBookmarkStore((s) => s.setPinnedBookmark);

  useEffect(() => {
    if (!targetMessageTs || !containerRef.current) return;

    // Find the index of the target message in our virtual items
    const targetIdx = virtualItems.findIndex(
      (item) => item.type === "message" && item.entry.ts === targetMessageTs
    );

    if (targetIdx !== -1) {
      setPinnedBookmark(null);
      virtualizer.scrollToIndex(targetIdx, { align: "center" });

      // Highlight after scroll settles — the element may not be in the DOM yet
      setTimeout(() => {
        const el = containerRef.current?.querySelector(`[data-ts="${CSS.escape(targetMessageTs)}"]`);
        if (el) {
          el.classList.add("bookmark-highlight");
          setTimeout(() => el.classList.remove("bookmark-highlight"), 1500);
        }
      }, 300);

      clearScrollTarget();
      return;
    }

    // Message not loaded — show the bookmark as a pinned overlay
    if (targetBookmark) {
      setPinnedBookmark({
        ts: targetBookmark.message_ts,
        content: targetBookmark.message_content,
        role: targetBookmark.message_role,
      });
    }
    clearScrollTarget();
  }, [targetMessageTs, virtualItems, clearScrollTarget, targetBookmark, setPinnedBookmark, virtualizer]);

  // Search result navigation
  const searchNavigateTarget = useSearchStore((s) => s.navigateTarget);
  const clearNavigateTarget = useSearchStore((s) => s.setNavigateTarget);
  const setPinnedSearchResult = useSearchStore((s) => s.setPinnedSearchResult);

  useEffect(() => {
    if (!searchNavigateTarget || !containerRef.current) return;
    if (searchNavigateTarget.agentId !== selectedAgentId) return;
    if (messagesLoading) return;

    setPinnedSearchResult({
      ts: searchNavigateTarget.ts,
      content: searchNavigateTarget.content,
      role: searchNavigateTarget.role,
      agentName: searchNavigateTarget.agentName,
    });
    clearNavigateTarget(null);
  }, [searchNavigateTarget, selectedAgentId, messages, messagesLoading, clearNavigateTarget, setPinnedSearchResult]);

  // Preserve scroll position after messages are prepended by load-more.
  // We snapshot the item count when load-more is triggered (in maybeLoadOlder),
  // then use that snapshot to compute the shift once loading completes.
  const preLoadItemCountRef = useRef<number | null>(null);
  // Counts consecutive auto-triggered fetches while the container isn't
  // scrollable (see `maybeLoadOlder`'s `notScrollable` branch below).
  const autoBackfillCountRef = useRef(0);

  /** Fire load-more once the top of the list is reached. Three signals, any
   *  one sufficient:
   *
   *  1. The container isn't scrollable at all
   *     (`scrollHeight <= clientHeight`). A short thread, or one whose recent
   *     replies are collapsed behind "Read more", never overflows the
   *     viewport, so no `scroll` event can ever fire — without this check
   *     the fetch would never arm and the user would be stuck expanding
   *     replies by hand until the content happened to grow tall enough to
   *     scroll.
   *  2. The raw pixel gap (`scrollTop < 50`, cheap and usually right).
   *  3. The first *rendered* item's virtual index (`<= 1`). The index check
   *     exists because `estimateSize`'s bucketed guess (72/160/280px) is
   *     routinely wrong, and tanstack-virtual silently corrects `scrollTop` as
   *     rows above the viewport get their real height measured — the same
   *     "clamped up a few pixels" drift `scrollToBottom` above has to fight
   *     with a re-pin loop. Near the top that drift can push `scrollTop` back
   *     past 50px right after the user's scroll gesture settles, with no
   *     further `scroll` event to re-check it. Index position isn't affected
   *     by that pixel drift, so it stays reliable even when the pixel signal
   *     briefly lies.
   *
   *  Deliberately reads `virtualizer.range` (the actual visible window,
   *  binary-searched off live scroll offset) rather than
   *  `getVirtualItems()[0].index` — the latter includes the 15-item overscan
   *  buffer on both sides, so on a short thread (whole list fits in one
   *  screen + overscan) index 0 is *always* rendered regardless of scroll
   *  position, which would misfire "near top" while sitting at the bottom. */
  const maybeLoadOlder = useCallback(() => {
    const el = containerRef.current;
    if (!el) return;
    const { shouldFetch, nextAutoBackfillCount } = shouldLoadOlderMessages({
      hasMoreMessages,
      loadingMore,
      loadMoreInFlight: loadMoreInFlightRef.current,
      loadMoreCooldown: loadMoreCooldownRef.current,
      scrollTop: el.scrollTop,
      scrollHeight: el.scrollHeight,
      clientHeight: el.clientHeight,
      nearTopByIndex: (virtualizer.range?.startIndex ?? 0) <= 1,
      autoBackfillCount: autoBackfillCountRef.current,
    });
    autoBackfillCountRef.current = nextAutoBackfillCount;
    if (!shouldFetch) return;
    // Snapshot current state before load-more so scroll preservation can compute shift
    const firstVisible = virtualizer.getVirtualItems()[0];
    loadMoreAnchorIndexRef.current = firstVisible ? firstVisible.index : 0;
    preLoadItemCountRef.current = virtualItems.length;
    loadMoreInFlightRef.current = true;
    loadMoreMessages();
  }, [hasMoreMessages, loadingMore, loadMoreMessages, virtualizer, virtualItems.length]);

  useLayoutEffect(() => {
    if (loadMoreAnchorIndexRef.current !== null && !loadingMore) {
      const anchorIndex = loadMoreAnchorIndexRef.current;
      const preLoadCount = preLoadItemCountRef.current ?? virtualItems.length;
      loadMoreAnchorIndexRef.current = null;
      preLoadItemCountRef.current = null;
      const shift = virtualItems.length - preLoadCount;
      if (shift > 0) {
        virtualizer.scrollToIndex(anchorIndex + shift, { align: "start" });
      }
      // Defer clearing the in-flight flag so the auto-scroll effect (same render)
      // and any scroll events from scrollToIndex still see it as true.
      // Also set a cooldown to prevent the programmatic scrollToIndex from
      // immediately re-triggering load-more if it lands near the top.
      loadMoreCooldownRef.current = true;
      requestAnimationFrame(() => {
        loadMoreInFlightRef.current = false;
        // Allow another short frame for scroll events from scrollToIndex to settle
        requestAnimationFrame(() => {
          loadMoreCooldownRef.current = false;
          // Clearing the refs above doesn't itself trigger a re-render, so
          // nothing would otherwise re-check the top until some unrelated
          // render happened to fire. Re-check here so a non-overflowing
          // thread keeps backfilling on its own until it either fills the
          // viewport or runs out of history.
          maybeLoadOlder();
        });
      });
    }
  }, [virtualItems.length, loadingMore, virtualizer]);

  // Re-run the near-top check whenever the virtualizer's measured total size
  // changes — not just on native `scroll` events. A measurement correction
  // (see `maybeLoadOlder` above) can happen without any further user scroll,
  // so relying on `onScroll` alone leaves a window where the list is
  // genuinely at the top but nothing re-checks it. Also covers the very
  // first render, arming the not-scrollable backfill before any scroll or
  // load-more has happened.
  const totalSize = virtualizer.getTotalSize();
  useEffect(() => {
    maybeLoadOlder();
  }, [totalSize, maybeLoadOlder]);

  // Primary older-history trigger: a level-triggered IntersectionObserver.
  //
  // Every path above is edge-triggered — the scroll handler needs a fresh
  // `scroll` event landing inside a narrow near-top threshold, and the
  // `totalSize` / Fix-2 re-checks need a measurement change or a cooldown
  // clearing. A user who flicks the list up and stops a little short of the
  // very top produces none of those: no further scroll event fires, the
  // measured total size doesn't change, and `virtualizer.range` may still read
  // "not near top" — so `maybeLoadOlder` never re-arms and the older page
  // silently fails to load. That is the intermittent symptom.
  //
  // Observing a sentinel pinned at content-offset 0 fires purely on that
  // sentinel becoming visible (with a prefetch `rootMargin`), independent of
  // scroll events, cooldown clearing, or re-renders — structurally immune to
  // the edge-triggered dead zone. It funnels through the SAME `maybeLoadOlder`
  // so every in-flight / cooldown / backfill-cap guard still applies and we
  // never double-fetch. A ref indirection keeps the observer (set up once)
  // calling the latest `maybeLoadOlder` closure without re-creating it on every
  // `virtualItems.length` change.
  const maybeLoadOlderRef = useRef(maybeLoadOlder);
  useEffect(() => {
    maybeLoadOlderRef.current = maybeLoadOlder;
  }, [maybeLoadOlder]);

  useEffect(() => {
    const root = containerRef.current;
    const sentinel = topSentinelRef.current;
    if (!root || !sentinel) return;
    const observer = new IntersectionObserver(
      (entries) => {
        const entry = entries[entries.length - 1];
        if (!entry) return;
        if (
          shouldObserverTriggerLoad({
            isIntersecting: entry.isIntersecting,
            observerReady: observerReadyRef.current,
          })
        ) {
          maybeLoadOlderRef.current();
        }
      },
      // Root is the scroll container so the sentinel is measured against the
      // live viewport; the top `rootMargin` pre-fetches slightly before the
      // true top scrolls fully into view.
      { root, rootMargin: "300px 0px 0px 0px", threshold: 0 }
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
    // Both nodes are stable for the component's lifetime (unconditional render,
    // remounts on agent switch), so a one-time setup/teardown is correct.
  }, []);

  const handleScroll = useCallback(() => {
    const el = containerRef.current;
    if (!el) return;

    // Track whether user is near bottom (for auto-scroll decision)
    wasNearBottomRef.current = isNearBottom();
    setShowScrollToBottom(!wasNearBottomRef.current);
    // Only hide the "is responding" pill if the user actually scrolled up.
    // Otherwise a streaming token that grows the bubble past the near-bottom
    // threshold flashes the pill off for one frame before auto-scroll catches up.
    const scrolledUp = el.scrollTop < lastScrollTopRef.current - 4;
    lastScrollTopRef.current = el.scrollTop;
    if (wasNearBottomRef.current || scrolledUp) {
      setStreamingAreaVisible(wasNearBottomRef.current);
    }

    maybeLoadOlder();

    // Throttle the sticky date DOM queries to avoid jank
    if (stickyRafRef.current) return;
    stickyRafRef.current = requestAnimationFrame(() => {
      stickyRafRef.current = null;
      if (!containerRef.current) return;

      const messageEls = containerRef.current.querySelectorAll("[data-date]");
      let topDate: string | null = null;
      const containerTop = containerRef.current.getBoundingClientRect().top;
      for (const msgEl of messageEls) {
        const rect = msgEl.getBoundingClientRect();
        if (rect.bottom > containerTop) {
          topDate = (msgEl as HTMLElement).dataset.date ?? null;
          break;
        }
      }

      if (topDate) {
        setStickyDate(topDate);
        setStickyVisible(true);
      }

      if (scrollTimeoutRef.current) clearTimeout(scrollTimeoutRef.current);
      scrollTimeoutRef.current = setTimeout(() => {
        setStickyVisible(false);
      }, 1200);
    });
  }, [isNearBottom, maybeLoadOlder]);

  return (
    <div className="relative flex-1 flex flex-col min-h-0">
    <div
      ref={containerRef}
      onScroll={handleScroll}
      data-scroll-container
      style={{ overflowAnchor: "auto" }}
      // Extra top padding clears the floating tab strip that overlays the top
      // of the conversation, so the first bubble starts just below it (content
      // still scrolls under the frosted bar as you scroll up).
      className="relative flex-1 overflow-y-auto overflow-x-hidden px-[10px] @3xl:px-[16px] @5xl:px-[20px] pt-[52px] pb-[16px]"
    >
      {/* Sticky date bubble */}
      {stickyDate && (
        <div
          className="pointer-events-none sticky top-[48px] z-10 flex justify-center py-[4px] transition-opacity duration-300"
          style={{ opacity: stickyVisible ? 1 : 0, overflowAnchor: "none" }}
        >
          <span className="px-[12px] py-[4px] rounded-full bg-[var(--bg-hover)]/90 text-[13px] font-bold text-[var(--text-primary)] select-none backdrop-blur-sm shadow-sm">
            {formatDateLabel(stickyDate)}
          </span>
        </div>
      )}

      {/* Loading spinner overlay — absolute so it never shifts content */}
      {loadingMore && (
        <div className="pointer-events-none absolute left-0 right-0 top-[80px] z-10 flex justify-center">
          <span className="px-[10px] py-[4px] rounded-full bg-[var(--bg-hover)]/90 backdrop-blur-sm shadow-sm flex items-center gap-[6px]">
            <div className="h-[14px] w-[14px] animate-spin rounded-full border-2 border-[var(--border-primary)] border-t-[var(--accent)]" />
            <span className="text-[12px] text-[var(--text-secondary)] select-none">Loading</span>
          </span>
        </div>
      )}

      {/* Virtualized list container — flow layout with spacers for native scroll anchoring */}
      {(() => {
        const items = virtualizer.getVirtualItems();
        const topSpace = items.length > 0 ? items[0].start : 0;
        const bottomSpace = items.length > 0
          ? virtualizer.getTotalSize() - items[items.length - 1].end
          : virtualizer.getTotalSize();
        return (
          <div className="flex flex-col">
            {/* Top-of-history sentinel — sits at content-offset 0 (above the
                virtual top spacer, i.e. the true top of all loaded history).
                The IntersectionObserver effect fires `maybeLoadOlder` when this
                scrolls into view, giving a level-triggered older-page load that
                can't fall into the edge-triggered scroll-event dead zone. The
                constant 1px height is uniform across prepends, so it never
                perturbs the scroll-anchor-on-prepend delta; `overflowAnchor:
                none` keeps it out of the browser's own scroll anchoring. */}
            <div
              ref={topSentinelRef}
              data-top-sentinel
              aria-hidden
              style={{ height: 1, flexShrink: 0, overflowAnchor: "none" }}
            />
            <div style={{ height: topSpace, flexShrink: 0, overflowAnchor: "none" }} />
            {items.map((vRow) => {
              const item = virtualItems[vRow.index];
              const nextItem = virtualItems[vRow.index + 1];
              // Tighten bottom spacing when the next item continues this sender's group
              const tightBottom = nextItem?.type === "message" && nextItem.groupWithPrevious;

              return (
                <div
                  key={vRow.key}
                  data-index={vRow.index}
                  ref={virtualizer.measureElement}
                  className={tightBottom ? "pb-[2px]" : "pb-[12px]"}
                  style={{ overflow: "hidden" }}
                >
                  {item.type === "message" && item.showForkDivider && <ForkDivider />}
                  {item.type === "streaming" ? (
                    <StreamingMessage prefixSegments={item.prefixSegments} />
                  ) : item.entry.event_type === "form_request" ? (
                    <div data-date={item.dateStr} data-ts={item.entry.ts}>
                      {item.showSeparator && <DateSeparator dateStr={item.dateStr} />}
                      {(() => {
                        const md = item.entry.metadata as AsyncFormRequestMeta | null | undefined;
                        if (!md?.spec) return null;
                        const agId = selectedAgentId ?? "";
                        const formId = md.spec.form_id;
                        const answers = answersByFormId.get(formId);
                        const branch = resolveFormRequestBranch({
                          isPending: md.form_id === pendingFormId,
                          isDismissed: dismissedFormIds.has(formId),
                          isAnswered: answers !== undefined,
                        });
                        // Pending forms are shown in the bottom overlay (ChatView),
                        // not inline — render a slim placeholder here instead.
                        if (branch === "pending") {
                          return (
                            <div className="py-[8px] px-[12px] rounded-[10px] bg-[var(--bg-secondary)] text-[13px] text-[var(--text-secondary)] flex items-center gap-[8px]">
                              <span>📋</span>
                              <span>Waiting for your response…</span>
                            </div>
                          );
                        }
                        // Declined forms surface via the form_dismissed indicator.
                        if (branch === "dismissed") return null;
                        // Answered (or just-submitted) form: render a read-only,
                        // pre-filled bubble on the user's side of the chat.
                        if (branch === "answered") {
                          const payload: FormRequestPayload = {
                            form_id: formId,
                            agent_id: agId,
                            session_id: "",
                            title: md.spec.title,
                            intro: md.spec.intro ?? undefined,
                            fields: md.spec.fields,
                          };
                          return (
                            <AskUserQuestionForm
                              agentId={agId}
                              form={payload}
                              onSubmit={async () => {}}
                              readOnly
                              initialAnswers={answers}
                            />
                          );
                        }
                        // Unanswered, not dismissed, and no longer the current
                        // pending form (something else happened in the thread
                        // since it posted) — the backend's `async_form_answer`
                        // route only checks plain pending-form membership, not
                        // latestness, so a late answer still lands. Stay fully
                        // interactive here, wired to the same submit/dismiss
                        // endpoints (and the same `meta`-driven payload shape
                        // via `AsyncFormRequestCard`) the floating overlay uses
                        // — instead of degrading into a blank, unanswerable
                        // read-only bubble.
                        const target = resolveInlineAsyncFormTarget(selectedAgentId);
                        return (
                          <AsyncFormRequestCard
                            meta={md}
                            agentId={agId}
                            onSubmit={async (values) => {
                              if (target.kind === "project") {
                                await submitAsyncFormAnswerForProject(target.projectId, formId, values);
                                if (pendingAsyncFormIdByChannel[selectedAgentId ?? ""] === formId) {
                                  clearPendingAsyncFormId(selectedAgentId ?? "");
                                }
                              } else if (target.kind === "agent") {
                                await submitAsyncFormAnswer(target.agentId, formId, values);
                                clearPendingAsyncForm(target.agentId, formId);
                              }
                            }}
                            // Exit affordance — Cancel / Regenerate / Something
                            // else all collapse onto the same dismiss the old
                            // standalone Decline button used here: nothing is
                            // suspended server-side for an inline, fallen-out-
                            // of-"current" async form to differentiate them
                            // against (`_action`/`_note` unused for the same
                            // reason).
                            onAction={async (_action, _note) => {
                              if (target.kind === "project") {
                                await dismissAsyncFormForProject(target.projectId, formId);
                                if (pendingAsyncFormIdByChannel[selectedAgentId ?? ""] === formId) {
                                  clearPendingAsyncFormId(selectedAgentId ?? "");
                                }
                              } else if (target.kind === "agent") {
                                await dismissAsyncForm(target.agentId, formId);
                                clearPendingAsyncForm(target.agentId, formId);
                              }
                            }}
                          />
                        );
                      })()}
                    </div>
                  ) : item.entry.event_type === "form_dismissed" ? (
                    <div data-date={item.dateStr} data-ts={item.entry.ts}>
                      {item.showSeparator && <DateSeparator dateStr={item.dateStr} />}
                      <FormDismissedIndicator />
                    </div>
                  ) : item.entry.event_type === "form_withdrawn" ? (
                    <div data-date={item.dateStr} data-ts={item.entry.ts}>
                      {item.showSeparator && <DateSeparator dateStr={item.dateStr} />}
                      {(() => {
                        // `metadata.form_id` — see `form_withdrawn_entry`,
                        // crates/ao-engine-tools-core/src/form_events.rs —
                        // is the withdrawn form's own id, used purely to look
                        // up a client-side recoverable draft (see
                        // `FormWithdrawnIndicator`'s `formId` prop doc).
                        const md = item.entry.metadata as Record<string, unknown> | null | undefined;
                        const formId = typeof md?.form_id === "string" ? md.form_id : undefined;
                        return <FormWithdrawnIndicator content={item.entry.content} formId={formId} />;
                      })()}
                    </div>
                  ) : item.entry.event_type === "form_answer" ? (
                    <div data-date={item.dateStr} data-ts={item.entry.ts}>
                      {item.showSeparator && <DateSeparator dateStr={item.dateStr} />}
                      {(() => {
                        const md = item.entry.metadata as Record<string, unknown> | null | undefined;
                        // Sync forms (AskUserQuestionWithForm) have no separate
                        // form_request entry — render the read-only bubble here.
                        if (md?.form) {
                          // The operator clicked an action button instead of
                          // submitting — there are no answers to show, so a
                          // small indicator reads better than a blank filled-in
                          // form (see FormActionIndicator's docs).
                          if (md.action) {
                            return <FormActionIndicator action={md.action as FormAction} />;
                          }
                          return (
                            <AskUserQuestionForm
                              agentId={selectedAgentId ?? ""}
                              form={md.form as FormRequestPayload}
                              onSubmit={async () => {}}
                              readOnly
                              initialAnswers={md.answers as FormAnswerMap}
                            />
                          );
                        }
                        if (md?.form_id !== undefined && md?.values !== undefined) {
                          // Spec snapshotted onto THIS entry at answer time
                          // (see the backend's `FormAnswerMeta.spec`) — the
                          // entry is fully self-contained, so render it as
                          // the SAME interactive form component the operator
                          // answered, disabled and filled in. Deliberately
                          // never resolved via a join against a separate
                          // `form_request` entry or the live `pending_forms`
                          // registry: forms get superseded and withdrawn, so
                          // a join could point at the wrong form, or none.
                          const form = buildAnsweredAsyncForm(md.form_id, md.spec as AsyncFormSpec | undefined);
                          if (form) {
                            return (
                              <AskUserQuestionForm
                                agentId={selectedAgentId ?? ""}
                                form={form}
                                onSubmit={async () => {}}
                                readOnly
                                initialAnswers={md.values as FormAnswerMap}
                              />
                            );
                          }
                          // Legacy entry written before the spec snapshot
                          // existed — no spec to drive an interactive form
                          // from. Tidy values-only fallback; deliberately
                          // never falls back to `entry.content` here — a
                          // pre-fix `content` string can itself carry the
                          // raw envelope JSON / "(no answer)" text this
                          // whole change exists to stop leaking into the UI.
                          return <AsyncFormAnswerBubble meta={md as unknown as AsyncFormAnswerMeta} />;
                        }
                        return null;
                      })()}
                    </div>
                  ) : item.entry.event_type === "todo_list_complete" ? (
                    <div data-date={item.dateStr} data-ts={item.entry.ts}>
                      {item.showSeparator && <DateSeparator dateStr={item.dateStr} />}
                      <div className="flex justify-center py-[6px] px-[16px]">
                        <span className="inline-flex items-center gap-[6px] text-[11px] text-[var(--text-tertiary)] bg-[var(--bg-tertiary)] rounded-full px-[10px] py-[4px] max-w-[80%]">
                          <ListChecks size={12} className="shrink-0 opacity-60" />
                          <span className="truncate">{item.entry.content}</span>
                        </span>
                      </div>
                    </div>
                  ) : item.entry.event_type === "delegate_complete" ? (() => {
                    const status =
                      (item.entry.metadata?.status as string | undefined) ?? "completed";
                    const isFailed = status === "failed";
                    const isCancelled = status === "cancelled";
                    const Icon = isFailed ? AlertCircle : ArrowRight;
                    const colorClass = isFailed
                      ? "text-[var(--text-danger,#e53e3e)]"
                      : isCancelled
                        ? "text-[var(--text-tertiary)]"
                        : "text-[var(--text-tertiary)]";
                    return (
                      <div data-date={item.dateStr} data-ts={item.entry.ts}>
                        {item.showSeparator && <DateSeparator dateStr={item.dateStr} />}
                        <div className="flex justify-center py-[6px] px-[16px]">
                          <span className={`inline-flex items-center gap-[6px] text-[11px] ${colorClass} bg-[var(--bg-tertiary)] rounded-full px-[10px] py-[4px] max-w-[80%]`}>
                            <Icon size={12} className="shrink-0 opacity-60" />
                            <span className="truncate">{item.entry.content}</span>
                          </span>
                        </div>
                      </div>
                    );
                  })() : item.entry.role === "system" ? (() => {
                    const severity = item.entry.metadata?.severity;
                    const toneClass = systemMessageToneClass(severity);
                    return (
                      <div data-date={item.dateStr} data-ts={item.entry.ts}>
                        {item.showSeparator && <DateSeparator dateStr={item.dateStr} />}
                        <div className="flex justify-center py-[6px] px-[16px]">
                          <span
                            className={`text-[12px] ${toneClass} rounded-[8px] px-[12px] py-[6px] text-center max-w-[80%]`}
                            data-severity={typeof severity === "string" ? severity : "neutral"}
                          >
                            {item.entry.content}
                          </span>
                        </div>
                      </div>
                    );
                  })() : isScheduleMessage(item.entry) ? (
                    <div data-date={item.dateStr} data-ts={item.entry.ts}>
                      {item.showSeparator && <DateSeparator dateStr={item.dateStr} />}
                      <div className="flex justify-center py-[6px] px-[16px]">
                        <span className="inline-flex items-center gap-[6px] text-[11px] text-[var(--text-tertiary)] bg-[var(--bg-tertiary)] rounded-full px-[10px] py-[4px] max-w-[80%]">
                          <Calendar size={12} className="shrink-0 opacity-60" />
                          <span className="truncate">
                            Scheduled item triggered
                            {item.entry.content ? ` — ${item.entry.content}` : ""}
                          </span>
                        </span>
                      </div>
                    </div>
                  ) : (
                    <div data-date={item.dateStr} data-ts={item.entry.ts}>
                      {item.showSeparator && <DateSeparator dateStr={item.dateStr} />}
                      <MessageBubble
                        entry={item.entry}
                        agentName={agentName}
                        agentEmoji={agentEmoji}
                        agentId={selectedAgentId ?? undefined}
                        groupWithPrevious={item.groupWithPrevious}
                        coalescedSegments={item.coalescedSegments}
                        allowBranch={allowBranch}
                        artifactIds={item.artifactIds}
                      />
                    </div>
                  )}
                </div>
              );
            })}
            <div style={{ height: bottomSpace, flexShrink: 0, overflowAnchor: "none" }} />
          </div>
        );
      })()}

    </div>

      {/* Scroll to bottom button — rendered outside the scroll container so
          mounting/unmounting it can't alter scrollHeight and clamp scrollTop. */}
      <AnimatePresence>
        {showScrollToBottom && (
          <motion.button
            initial={{ opacity: 0, scale: 0.8 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.8 }}
            transition={{ duration: 0.15 }}
            onClick={() => {
              scrollToBottom();
              setShowScrollToBottom(false);
            }}
            className="absolute bottom-4 left-1/2 -translate-x-1/2 z-10 w-[36px] h-[36px] rounded-full bg-[var(--bg-hover)] border border-[var(--border-secondary)] shadow-md flex items-center justify-center text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--bg-tertiary)] transition-colors cursor-pointer"
          >
            <ChevronDown className="w-[18px] h-[18px]" />
          </motion.button>
        )}
      </AnimatePresence>
    </div>
  );
}

/** Pinned bookmark overlay — renders outside the scroll container */
export function PinnedBookmarkOverlay() {
  const pinnedBookmark = useBookmarkStore((s) => s.pinnedBookmark);
  const setPinnedBookmark = useBookmarkStore((s) => s.setPinnedBookmark);
  const selectedAgentId = useChatStore((s) => s.selectedAgentId);
  const resolved = useResolvedAgent(selectedAgentId ?? "");
  const agentName = resolved.displayName || "Agent";
  const agentEmoji = resolved.emoji;

  return (
    <AnimatePresence>
      {pinnedBookmark && (
        <motion.div
          initial={{ y: "100%" }}
          animate={{ y: 0 }}
          exit={{ y: "100%" }}
          transition={{ type: "spring", damping: 30, stiffness: 350 }}
          className="absolute inset-0 z-30 bg-[var(--bg-secondary)] flex flex-col"
        >
          <div className="flex-shrink-0 px-[16px] pt-[12px] pb-[8px]">
            <div className="flex items-center justify-between">
              <span className="text-[13px] text-[var(--text-secondary)] flex items-center gap-[6px]">
                <Bookmark className="w-[13px] h-[13px]" />
                Bookmarked message
              </span>
              <button
                onClick={() => setPinnedBookmark(null)}
                className="text-[12px] text-[var(--text-tertiary)] hover:text-[var(--text-primary)] px-[8px] py-[4px] rounded-[6px] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
              >
                Dismiss
              </button>
            </div>
            <div className="h-px bg-[var(--border-secondary)] mt-[8px]" />
          </div>
          <div className="flex-1 overflow-y-auto px-[10px] pb-[16px]">
            <MessageBubble
              entry={{
                ts: pinnedBookmark.ts,
                role: pinnedBookmark.role,
                content: pinnedBookmark.content,
                event_type: "message",
              }}
              agentName={agentName}
              agentEmoji={agentEmoji}
              agentId={selectedAgentId ?? undefined}
            />
          </div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}

/** Pinned search result overlay — renders outside the scroll container */
export function PinnedSearchOverlay() {
  const pinnedSearchResult = useSearchStore((s) => s.pinnedSearchResult);
  const setPinnedSearchResult = useSearchStore((s) => s.setPinnedSearchResult);
  const selectedAgentId = useChatStore((s) => s.selectedAgentId);
  const resolved = useResolvedAgent(selectedAgentId ?? "");
  // For deleted agents, prefer the name captured in the search result hit over
  // the truncated-id fallback so the pinned result still reads naturally.
  const agentName = resolved.isFallback
    ? (pinnedSearchResult?.agentName ?? resolved.displayName ?? "Agent")
    : resolved.displayName;
  const agentEmoji = resolved.emoji;

  return (
    <AnimatePresence>
      {pinnedSearchResult && (
        <motion.div
          initial={{ y: "100%" }}
          animate={{ y: 0 }}
          exit={{ y: "100%" }}
          transition={{ type: "spring", damping: 30, stiffness: 350 }}
          className="absolute inset-0 z-30 bg-[var(--bg-secondary)] flex flex-col"
        >
          <div className="flex-shrink-0">
            <div className="flex items-center justify-between px-[16px] pt-[12px] pb-[8px]">
              <span className="text-[13px] text-[var(--text-secondary)] flex items-center gap-[6px]">
                <Search className="w-[13px] h-[13px]" />
                Search result
              </span>
              <button
                onClick={() => setPinnedSearchResult(null)}
                className="text-[12px] text-[var(--text-tertiary)] hover:text-[var(--text-primary)] px-[8px] py-[4px] rounded-[6px] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
              >
                Dismiss
              </button>
            </div>
            <div className="h-px bg-[var(--border-secondary)]" />
          </div>
          <div className="flex-1 overflow-y-auto px-[10px] pt-[12px] pb-[16px]">
            <MessageBubble
              entry={{
                ts: pinnedSearchResult.ts,
                role: pinnedSearchResult.role,
                content: pinnedSearchResult.content,
                event_type: "message",
              }}
              agentName={agentName}
              agentEmoji={agentEmoji}
              agentId={selectedAgentId ?? undefined}
            />
          </div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
