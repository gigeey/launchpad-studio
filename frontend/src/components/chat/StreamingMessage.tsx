import { memo, useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { ArrowUp, ArrowDown, Zap, Sparkles, Lightbulb, LightbulbOff, ChevronDown, ChevronRight } from "lucide-react";
import {
  useStreamingText,
  useActiveToolCalls,
  useTurnUsage,
  useInFlightThinking,
  useInFlightArtifactIds,
  useStreamingThreadId,
  useIsAgentTurnActive,
  useEverShownThisTurn,
  useChatStore,
  type InFlightThinking,
  type TurnUsage,
} from "../../stores/chatStore";
import { useIsDark, useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { agentAvatarColor } from "../../lib/agentColors";
import { ArtifactCardTile, SkillLoadChip, type CoalescedSegment } from "./MessageBubble";

import { stripMcpPrefix, describeToolCall } from "./toolCallLabel";
export { stripMcpPrefix, describeToolCall } from "./toolCallLabel";

/** Format elapsed seconds into a human-readable string */
function formatElapsed(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const mins = Math.floor(seconds / 60);
  const secs = seconds % 60;
  return `${mins}m ${secs}s`;
}

/** Rotating status messages for Agent tool calls */
const AGENT_STATUS_MESSAGES = [
  "Working…",
  "Cooking…",
  "Wrestling…",
  "Brewing…",
  "Tinkering…",
  "Marinating…",
  "Simmering…",
  "Chewing on it…",
  "Noodling…",
  "Percolating…",
  "Conjuring…",
  "Wrangling…",
];

/** A single L-shape tool indicator with shimmer effect and optional detail line.
 *
 *  Label wraps instead of ellipsis-truncating: several `describeToolCall`
 *  branches (Delegate target, skill name, TodoList name, the raw-tool-name
 *  fallback) interpolate model/user-supplied strings with no length cap, and
 *  the bubble starts at `min-w-[180px]` before it grows to fit streamed
 *  text — a single-line `whitespace-nowrap` + `text-ellipsis` label was
 *  silently cutting those off well before they'd have overflowed a
 *  full-width bubble. Wrapping trades the single-line "log line" look for
 *  never hiding real content; the connector glyph is a fixed-size decorative
 *  stub (not row-height-driven via a negative margin) so it can't clip a
 *  wrapped second line against the bubble's `overflow-hidden`. */
function ToolIndicator({ label, detail, accentColor }: { label: string; detail?: string; accentColor: string }) {
  const connectorColor = "#b0b8c1"; // soft gray like iMessage/Slack thread lines
  const labelColor = accentColor;
  const detailColor = "#7a8a9e"; // muted blue-gray, distinct from connector
  return (
    <div className="mt-[3px] flex items-start">
      {/* L-shape: vertical stroke + corner + horizontal stroke */}
      <div
        className="flex-shrink-0 mt-[2px]"
        style={{
          width: "14px",
          height: "14px",
          borderLeft: `2px solid ${connectorColor}`,
          borderBottom: `2px solid ${connectorColor}`,
          borderRadius: "0 0 0 4px",
        }}
      />
      {/* flex-auto (not flex-1/flex-basis:0) is load-bearing: a flex-basis:0
          child's contribution to its container's preferred (max-content)
          width is its *min-content* size per spec, and break-words text's
          min-content collapses to ~one word. With flex-1 that made this
          indicator report a tiny preferred width, so absent a wide
          streamingText paragraph to dominate the calc, the whole bubble's
          auto-width would collapse toward min-w-[180px] and this label
          wrapped awkwardly inside the too-narrow result. flex-auto keeps
          the grow/shrink-to-fill-row behavior but contributes real
          max-content width upstream, so the bubble sizes correctly even
          when tool indicators are the only content. */}
      <div className="min-w-0 flex-auto pl-[4px]">
        {/* Tool label */}
        <motion.span
          key={label}
          initial={{ opacity: 0, x: -4 }}
          animate={{ opacity: 1, x: 0 }}
          className="tool-shimmer text-[12px] font-medium leading-[16px] break-words"
          style={{ color: labelColor }}
        >
          {label}
        </motion.span>
        {detail && (
          <motion.div
            initial={{ opacity: 0, y: -2 }}
            animate={{ opacity: 1, y: 0 }}
            className="text-[11px] mt-[6px] break-words"
            style={{ color: detailColor }}
          >
            {detail}
          </motion.div>
        )}
      </div>
    </div>
  );
}

/** Enhanced indicator for Agent tool calls — shows elapsed time and rotating
 *  status. `done` freezes both the elapsed-time tick and the status-message
 *  rotation at their last value instead of stopping the component rendering
 *  altogether: the chip itself now lingers on-screen after its own
 *  `tool_call_completed` (see `markInFlightToolCallDone`), and without this
 *  freeze it would keep counting up / cycling through "Cooking…
 *  Percolating…" and actively imply the subagent is still working well
 *  after it actually finished — worse than the old pop-on-completion
 *  behavior, not just cosmetically different from it. */
function AgentToolIndicator({ detail, startedAt, accentColor, done }: { detail?: string; startedAt: number; accentColor: string; done?: boolean }) {
  const [elapsed, setElapsed] = useState(0);
  const [statusIdx, setStatusIdx] = useState(0);

  useEffect(() => {
    if (done) return;
    const timer = setInterval(() => {
      setElapsed(Math.floor((Date.now() - startedAt) / 1000));
    }, 1000);
    return () => clearInterval(timer);
  }, [startedAt, done]);

  // Rotate status message every 5 seconds, starting after 3s
  useEffect(() => {
    if (done || elapsed < 3) return;
    const idx = Math.floor((elapsed - 3) / 5) % AGENT_STATUS_MESSAGES.length;
    setStatusIdx(idx);
  }, [elapsed, done]);

  const connectorColor = "#b0b8c1";
  const detailColor = "#7a8a9e";

  return (
    <div className="mt-[3px]">
      <div className="flex" style={{ height: "16px" }}>
        <div
          className="flex-shrink-0"
          style={{
            width: "14px",
            height: "100%",
            borderLeft: `2px solid ${connectorColor}`,
            borderBottom: `2px solid ${connectorColor}`,
            borderRadius: "0 0 0 4px",
          }}
        />
        <motion.span
          key="agent-label"
          initial={{ opacity: 0, x: -4 }}
          animate={{ opacity: 1, x: 0 }}
          className="tool-shimmer self-end text-[12px] font-medium pl-[4px]"
          style={{ marginBottom: "-7px", color: accentColor }}
        >
          Using Agent
        </motion.span>
        {elapsed >= 2 && (
          <motion.span
            initial={{ opacity: 0 }}
            animate={{ opacity: 0.5 }}
            className="self-end text-[11px] pl-[6px] tabular-nums"
            style={{ marginBottom: "-7px", color: detailColor }}
          >
            {formatElapsed(elapsed)}
          </motion.span>
        )}
      </div>
      {detail && (
        <motion.div
          initial={{ opacity: 0, y: -2 }}
          animate={{ opacity: 1, y: 0 }}
          className="text-[11px] mt-[6px] pl-[18px]"
          style={{ color: detailColor }}
        >
          {detail}
        </motion.div>
      )}
      {elapsed >= 3 && (
        <motion.div
          key={statusIdx}
          initial={{ opacity: 0, y: 2 }}
          animate={{ opacity: 0.6, y: 0 }}
          exit={{ opacity: 0, y: -2 }}
          transition={{ duration: 0.3 }}
          className="text-[11px] mt-[3px] pl-[18px] italic"
          style={{ color: detailColor }}
        >
          {AGENT_STATUS_MESSAGES[statusIdx]}
        </motion.div>
      )}
    </div>
  );
}

/** Tweens a number toward `value` on every change with a rAF-driven ease-out
 *  curve so the displayed count rolls (e.g. 1 → 2 → 3 → 4 → 5) instead of
 *  snapping. First render shows `value` synchronously — keeps unit tests
 *  deterministic without timer mocks. Animation only kicks in for *subsequent*
 *  value changes (the streaming-update path). */
function AnimatedNumber({ value }: { value: number }) {
  const [display, setDisplay] = useState(value);
  // Holds the value we most recently animated to. We compare against this on
  // each re-render to decide whether to start a new tween, so re-renders with
  // an unchanged `value` don't restart the animation.
  const prevValueRef = useRef(value);

  useEffect(() => {
    const from = prevValueRef.current;
    const to = value;
    if (from === to) return;
    // ~450ms feels brisk for small deltas (1→5) and still readable for big
    // jumps (0→145k). Ease-out so the count slows at the end — gives the
    // "settling" feel users expect from a running counter.
    const duration = 450;
    const start = performance.now();
    let raf = 0;
    const tick = (now: number) => {
      const t = Math.min(1, (now - start) / duration);
      const eased = 1 - Math.pow(1 - t, 3);
      setDisplay(Math.round(from + (to - from) * eased));
      if (t < 1) raf = requestAnimationFrame(tick);
      else prevValueRef.current = to;
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [value]);

  return <>{display.toLocaleString()}</>;
}

/** Token-count strip rendered above the tool-call chips. The input (ArrowUp)
 *  and output (ArrowDown) fields always render — that's the headline cost
 *  signal users care about. The cache breakdown (Zap = cache_read,
 *  Sparkles = cache_creation) is the load-bearing dogfood signal that prompt
 *  caching is paying off, but it's noisy for end users — so it's gated behind
 *  `import.meta.env.DEV` (Vite's dev-build flag). Production builds get the
 *  lean two-field strip; dev builds get the full four-field readout for
 *  telemetry verification.
 *
 *  Icons (lucide-react) replace the earlier emoji glyphs so the strip blends
 *  with the rest of the chat UI instead of standing out as a different visual
 *  register. Each field carries a `data-field` attribute so tests can target
 *  fields without depending on glyph text (the icon renders as an SVG node).
 *
 *  Exported so the unit test can render it in isolation without booting the
 *  full streaming bubble. `showCacheFields` defaults to `import.meta.env.DEV`
 *  and is overridable for tests that need to pin either branch deterministically. */
export function UsageStrip({
  usage,
  accentColor,
  showCacheFields = import.meta.env.DEV,
}: {
  usage: TurnUsage;
  accentColor: string;
  showCacheFields?: boolean;
}) {
  // Express cache_read as a share of total input so the cache-hit signal is
  // legible at a glance. Only shown when the cache fields themselves are
  // visible — otherwise the percentage has nothing to anchor to.
  const totalInput = usage.input + usage.cacheRead + usage.cacheCreation;
  const cacheRatio = totalInput > 0 ? Math.round((usage.cacheRead / totalInput) * 100) : 0;
  // 12px keeps the icon optically balanced against text-xs (~12px line). The
  // matching 1.5px stroke is lucide's default — bumping it would compete with
  // the bold number text rather than supporting it.
  const iconProps = { size: 12, "aria-hidden": true } as const;
  return (
    <div
      // flex-wrap (not flex-nowrap) so the strip drops to a second line
      // instead of overflowing — the bubble that wraps this has
      // overflow-hidden, so a nowrap row simply clips the trailing field(s)
      // once the bubble is narrower than the strip's natural width (e.g. dev
      // mode's 4-field readout in a narrow bubble). whitespace-nowrap stays
      // off the container so wrapping is actually allowed; each field span
      // below is its own inline-flex unit, so icon+number pairs still move
      // to the next line together rather than splitting mid-field.
      className="flex flex-wrap items-center gap-x-[14px] gap-y-[4px] text-xs font-semibold mb-[10px] tabular-nums"
      style={{ color: accentColor, opacity: 0.9 }}
      data-testid="usage-strip"
    >
      <span
        className="inline-flex items-center gap-[5px]"
        title="Fresh input tokens"
        data-field="input"
      >
        <ArrowUp {...iconProps} />
        <AnimatedNumber value={usage.input} />
      </span>
      <span
        className="inline-flex items-center gap-[5px]"
        title="Output tokens"
        data-field="output"
      >
        <ArrowDown {...iconProps} />
        <AnimatedNumber value={usage.output} />
      </span>
      {showCacheFields && (
        <>
          <span
            className="inline-flex items-center gap-[5px]"
            title="Tokens read from prompt cache (cache hit)"
            data-field="cache-read"
          >
            <Zap {...iconProps} />
            <AnimatedNumber value={usage.cacheRead} />
            {totalInput > 0 ? <span>({cacheRatio}%)</span> : null}
          </span>
          {usage.cacheCreation > 0 && (
            <span
              className="inline-flex items-center gap-[5px]"
              title="Tokens newly written to the prompt cache this turn"
              data-field="cache-creation"
            >
              <Sparkles {...iconProps} />
              <AnimatedNumber value={usage.cacheCreation} />
            </span>
          )}
        </>
      )}
    </div>
  );
}

/** A compact "Thinking…" pill that sits at the top of the streaming bubble.
 *  Icon doubles as the state indicator — a lit `Lightbulb` while a block is
 *  active, a dim `LightbulbOff` otherwise — literally flicking on/off rather
 *  than mounting/unmounting, which is what used to make the bubble jump (see
 *  `thinkingShown` on `InFlightAgentMessage`). Four states drive the same
 *  component:
 *
 *  1. Active + no buffered text (provider runs with `display = "omitted"`):
 *     just a shimmering "Thinking…" label with the bulb lit. The whole point
 *     of this state is to explain the gap between the user pressing send and
 *     the first visible text token — so the pill is intentionally low-key.
 *  2. Active + buffered text (`display = "summarized"` or `raw`): same lit
 *     header, but expandable via a chevron to reveal the streaming
 *     reasoning text. Closed by default so the bubble stays uncluttered.
 *  3. Inactive (a block closed, whether or not it left buffered text):
 *     bulb goes dim and the header changes to "Thought for Ns". If the
 *     closed block had buffered text, the disclosure stays available.
 *  4. Inactive between two sequential thinking blocks in the same turn
 *     (long reasoning chains routinely open several): same dim "Thought for
 *     Ns" state as #3, showing the *last* block's duration — the pill stays
 *     mounted rather than disappearing, and re-lights the instant the next
 *     block's `thinking_started` arrives.
 *
 *  Persisted only on the in-flight entry. Once the run finalizes and the
 *  bubble collapses into the transcript, the pill is gone; thinking text
 *  doesn't survive into the final message bubble. */
const ThinkingPill = memo(function ThinkingPill({
  state,
  accentColor,
}: {
  state: InFlightThinking;
  accentColor: string;
}) {
  const [expanded, setExpanded] = useState(false);
  const detailColor = "#7a8a9e";
  const hasText = state.text.length > 0;
  // Live-tick the elapsed counter while the block is open. Initialised once
  // on mount from startedAt. The interval drives subsequent updates. When the
  // block is closed we read elapsedMs directly from the prop — no state sync
  // needed, which avoids the synchronous setState-in-effect pattern that can
  // push React past its 50-render nested-update limit.
  const [liveElapsed, setLiveElapsed] = useState<number>(() =>
    state.startedAt != null ? Date.now() - state.startedAt : 0
  );
  useEffect(() => {
    if (!state.active || state.startedAt == null) return;
    const id = setInterval(() => {
      setLiveElapsed(Date.now() - state.startedAt!);
    }, 250);
    return () => clearInterval(id);
  }, [state.active, state.startedAt]);

  // When the block is closed, prefer the server-reported elapsed value;
  // fall back to the last live reading if the server didn't send one.
  const displayElapsedMs = state.active ? liveElapsed : (state.elapsedMs ?? liveElapsed);
  const elapsedSeconds = Math.max(0, Math.floor(displayElapsedMs / 1000));
  const headerLabel = state.active
    ? "Thinking…"
    : `Thought${elapsedSeconds > 0 ? ` for ${elapsedSeconds}s` : ""}`;
  const canToggle = hasText;

  return (
    <div className="mb-[8px]">
      <button
        type="button"
        onClick={() => canToggle && setExpanded((v) => !v)}
        disabled={!canToggle}
        className={`inline-flex items-center gap-[5px] text-[11px] font-semibold px-[7px] py-[3px] rounded-full ${
          canToggle ? "cursor-pointer" : "cursor-default"
        }`}
        style={{
          backgroundColor: `color-mix(in srgb, ${accentColor} 12%, transparent)`,
          color: accentColor,
        }}
        data-testid="thinking-pill"
        data-active={state.active}
        data-expanded={expanded}
      >
        {state.active ? <Lightbulb size={11} aria-hidden /> : <LightbulbOff size={11} aria-hidden style={{ opacity: 0.6 }} />}
        <span className={state.active && !hasText ? "tool-shimmer" : ""}>{headerLabel}</span>
        {state.active && elapsedSeconds > 0 && (
          <span className="tabular-nums" style={{ opacity: 0.7 }}>
            {elapsedSeconds}s
          </span>
        )}
        {canToggle && (
          <span style={{ opacity: 0.7 }}>
            {expanded ? <ChevronDown size={11} aria-hidden /> : <ChevronRight size={11} aria-hidden />}
          </span>
        )}
      </button>
      {/* Live preview line — surfaces the tail of the reasoning buffer
          while the block is active and the disclosure is collapsed. Each
          time a new delta arrives the preview text updates, giving the
          user a continuous "model is still reasoning" heartbeat without
          forcing them to expand the disclosure. The fade-mask hides the
          left edge so partial-word truncation looks intentional rather
          than clipped. Hidden once the user manually expands (the full
          text is already visible) or once the block closes (no further
          deltas to surface). */}
      {state.active && hasText && !expanded && (
        <motion.div
          key={state.text.length}
          initial={{ opacity: 0 }}
          animate={{ opacity: 0.8 }}
          transition={{ duration: 0.18 }}
          className="mt-[4px] text-[13px] leading-[1.4] whitespace-nowrap overflow-hidden italic"
          style={{
            color: detailColor,
            // Fade the left edge so a mid-sentence start reads as
            // intentional. WebKit-only mask is sufficient — we ship to
            // a Tauri webview with a known engine.
            WebkitMaskImage:
              "linear-gradient(to right, transparent 0%, black 12%, black 100%)",
            maskImage:
              "linear-gradient(to right, transparent 0%, black 12%, black 100%)",
          }}
          data-testid="thinking-pill-preview"
        >
          {state.text.slice(-120)}
        </motion.div>
      )}
      <AnimatePresence initial={false}>
        {expanded && hasText && (
          <motion.div
            key="thinking-text"
            initial={{ opacity: 0, height: 0 }}
            animate={{ opacity: 1, height: "auto" }}
            exit={{ opacity: 0, height: 0 }}
            transition={{ duration: 0.18 }}
            className="overflow-hidden mt-[6px] text-[14px] leading-[1.45] whitespace-pre-wrap rounded-md px-[10px] py-[7px]"
            style={{
              color: detailColor,
              backgroundColor: `color-mix(in srgb, ${accentColor} 6%, transparent)`,
              border: `1px solid color-mix(in srgb, ${accentColor} 18%, transparent)`,
            }}
            data-testid="thinking-pill-text"
          >
            {state.text}
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
});

/** Lightweight markdown components for streaming — handles overflow on code/tables */
const streamingMdComponents = {
  code({ className, children }: { className?: string; children?: React.ReactNode }) {
    const text = typeof children === 'string' ? children : String(children ?? '');
    const hasNewlines = text.includes('\n');
    if (hasNewlines || className) {
      return (
        <code className={`${className ?? ''} block bg-[var(--bg-hover)] rounded-md px-3 py-2 my-2 text-[13px] whitespace-pre-wrap overflow-x-auto`}>
          {text}
        </code>
      );
    }
    return <code className="bg-[var(--bg-hover)] rounded px-1 py-0.5 text-[13px]">{text}</code>;
  },
  pre({ children }: { children?: React.ReactNode }) {
    return <>{children}</>;
  },
  table({ children }: { children?: React.ReactNode }) {
    return (
      <div className="overflow-x-auto my-2">
        <table className="text-[13px]">{children}</table>
      </div>
    );
  },
};

export function StreamingMessage({ prefixSegments }: { prefixSegments?: CoalescedSegment[] } = {}) {
  const selectedAgentId = useChatStore((s) => s.selectedAgentId);
  // Scope in-flight streaming state to the currently viewed thread — see
  // `useStreamingThreadId` for why the default thread collapses to
  // `undefined` rather than its real backend id.
  const streamingThreadId = useStreamingThreadId(selectedAgentId);
  const streamingText = useStreamingText(selectedAgentId, streamingThreadId);
  const activeToolCalls = useActiveToolCalls(selectedAgentId, streamingThreadId);
  const turnUsage = useTurnUsage(selectedAgentId, streamingThreadId);
  const thinking = useInFlightThinking(selectedAgentId, streamingThreadId);
  const artifactIds = useInFlightArtifactIds(selectedAgentId, streamingThreadId);
  const hasInFlightEntry = useIsAgentTurnActive(selectedAgentId, streamingThreadId);
  const everShownThisTurn = useEverShownThisTurn(selectedAgentId, streamingThreadId);
  const profile = useChatStore((s) => s.selectedAgentProfile);
  const isDark = useIsDark();
  const bubbleColor = useUserPreferencesStore((s) => s.bubbleColor);
  const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);

  const hasPrefix = (prefixSegments?.length ?? 0) > 0;
  const hasContent =
    !!streamingText || activeToolCalls.length > 0 || hasPrefix || !!thinking || artifactIds.length > 0;

  // Sticky "has this turn shown anything yet" latch. A thinking block that
  // runs with `display = "omitted"` flips `thinking` from a live object
  // straight to `null` the instant it closes (no buffered text to keep it
  // truthy) — if nothing else has started yet (another thinking block, a
  // tool call, or the reply text), `hasContent` would otherwise go false for
  // a beat and this component would unmount, then remount moments later.
  // That collapse/regrow is the "bubble jumps up and down" bug reported for
  // thinking-only gaps — fixed by keeping the bubble mounted (falling back
  // to the idle typing dots below) for the rest of the turn once it has
  // appeared, instead of re-deriving visibility from instantaneous content
  // alone.
  //
  // `everShownThisTurn` lives in the chatStore (`InFlightAgentMessage`)
  // rather than a local ref here — this component remounts on every
  // conversation switch (`MessageList key={deferredConversationKey}` in
  // ChatView), which used to reset a local ref to `false` and could blank
  // the entire bubble if the remount landed during a normal content gap
  // (e.g. between two tool calls). The store-backed latch survives that
  // remount and only resets when the in-flight entry itself tears down, so
  // the next turn still starts hidden like before.

  // Render only once the turn has produced actual content, tool calls, a
  // skill-load prefix carrying the bubble across a RunEnded → RunStarted
  // handoff, an in-flight thinking block, or an artifact card an
  // ArtifactWrite call has already produced this turn (so a card-only turn —
  // the reply text still streaming in — shows up immediately rather than
  // waiting for text) — or once any of those has already appeared earlier
  // in this same turn (see latch above).
  if (!hasInFlightEntry || (!hasContent && !everShownThisTurn)) {
    return null;
  }

  const emoji = profile?.emoji ?? "🤖";
  const agentName = profile?.name ?? "Agent";

  // Build per-tool indicators — each tool (including parallel agents) gets its own entry.
  // Normalize the tool name through `stripMcpPrefix` for the Agent branch so an
  // MCP-routed Agent call (`mcp__launchpad__Agent`) still picks up the
  // elapsed-time + rotating-status indicator variant rather than the generic chip.
  const indicators = activeToolCalls.map((tc) => ({
    ...(tc.label != null ? { label: tc.label } : describeToolCall(tc.tool, tc.input)),
    isAgent: stripMcpPrefix(tc.tool) === "Agent",
    startedAt: tc.startedAt,
    done: tc.done,
  }));

  return (
    <div className="flex items-start gap-[10px]">
      {/* Emoji avatar — matches final message style */}
      <div
        className={`flex-shrink-0 w-[34px] h-[34px] ${circularAvatars ? 'rounded-full' : 'rounded-[10px]'} flex items-center justify-center text-[18px] select-none mt-[2px]`}
        style={{ backgroundColor: agentAvatarColor(agentName, isDark) }}
      >
        {emoji}
      </div>

      {/* Bubble + name + typing dots */}
      <div className="flex flex-col items-start min-w-0">
        <div className="flex items-center gap-[6px] mb-[4px]">
          <span className="text-[14px] font-bold text-[var(--text-primary)]">{agentName}</span>
          <div className="flex items-center gap-[3px]">
            {[0, 1, 2].map((i) => (
              <motion.span
                key={i}
                className="block w-[4px] h-[4px] rounded-full bg-[var(--text-secondary)]"
                animate={{ opacity: [0.3, 1, 0.3] }}
                transition={{
                  duration: 1.2,
                  repeat: Infinity,
                  delay: i * 0.2,
                  ease: "easeInOut",
                }}
              />
            ))}
          </div>
        </div>

        <div
          className={`${
            // Match MessageBubble's finalized-bubble sizing so a card doesn't
            // visibly jump width the moment streaming hands off to the
            // persisted message (see MessageBubble.tsx for the full rationale).
            artifactIds.length > 0 ? "max-w-full" : "max-w-[95%] @3xl:max-w-[90%] @5xl:max-w-[85%]"
          } min-w-[180px] px-[14px] py-[10px] bg-[var(--bg-agent-message)] text-[15px] text-[var(--text-primary)] leading-[1.45] break-words overflow-hidden prose prose-sm prose-neutral ${indicators.length > 0 ? "pb-[14px]" : ""}`}
          style={{ borderRadius: "4px 16px 16px 16px" }}
        >
          {/* Renders the instant ArtifactWrite completes, mid-stream — no
              wait for text_complete/run_ended. Sits above the reply text,
              matching the finalized bubble's placement (MessageBubble). */}
          {selectedAgentId && artifactIds.length > 0 &&
            artifactIds.map((id) => <ArtifactCardTile key={id} artifactId={id} agentId={selectedAgentId} />)}
          {hasPrefix && prefixSegments!.map((seg, i) =>
            seg.kind === "text" ? (
              <Markdown key={`pre-${i}`} remarkPlugins={[remarkGfm]} components={streamingMdComponents}>{seg.content}</Markdown>
            ) : (
              <div key={`pre-${i}`}>
                <SkillLoadChip skillName={seg.skillName} success={seg.success} />
              </div>
            )
          )}
          {streamingText && (
            <Markdown remarkPlugins={[remarkGfm]} components={streamingMdComponents}>{streamingText}</Markdown>
          )}
          {/* Thinking pill sits below the message text, above the token-count
              strip. Renders for any of: live "Thinking…" (no buffered text),
              collapsible reasoning summary (buffered text + active), or
              collapsed "Thought for Ns" header after the block closes. */}
          {thinking && <ThinkingPill state={thinking} accentColor={bubbleColor} />}
          {/* Token-count strip — visible whenever a `usage` event has arrived
              for the current turn. Sits above the chips. */}
          {turnUsage && <UsageStrip usage={turnUsage} accentColor={bubbleColor} />}
          {/* Cursor — plain blink when idle, L-shape(s) into tool label(s) when
              active. Caps to the most recent 3 indicators, with a "+N more"
              label above when there are more — keeps the bubble from growing
              unbounded during long parallel tool-call bursts. */}
          {indicators.length > 0 ? (
            <>
              {indicators.length > 3 && (
                <div className="text-[11px] mb-[2px]" style={{ color: bubbleColor, opacity: 0.8 }}>
                  + {indicators.length - 3} more
                </div>
              )}
              {indicators.slice(-3).map((ind, i) =>
                ind.isAgent ? (
                  <AgentToolIndicator key={`tool-${i}`} detail={ind.detail} startedAt={ind.startedAt} accentColor={bubbleColor} done={ind.done} />
                ) : (
                  <ToolIndicator key={`tool-${i}`} label={ind.label} detail={ind.detail} accentColor={bubbleColor} />
                )
              )}
            </>
          ) : (
            <span className="inline-flex items-center gap-[3px] mt-[6px] align-middle">
              {[0, 1, 2].map((i) => (
                <motion.span
                  key={i}
                  className="block w-[5px] h-[5px] rounded-full bg-[var(--text-secondary)]"
                  animate={{ y: [0, -4, 0] }}
                  transition={{
                    duration: 0.6,
                    repeat: Infinity,
                    delay: i * 0.2,
                    ease: "easeInOut",
                  }}
                />
              ))}
            </span>
          )}
        </div>
      </div>

      {/* Shimmer animation keyframes — accent color driven by user bubble color */}
      <style>{`
        @keyframes shimmer-sweep {
          0% { background-position: -200% center; }
          100% { background-position: 200% center; }
        }
        .tool-shimmer {
          background: linear-gradient(
            90deg,
            ${bubbleColor} 0%,
            ${bubbleColor} 35%,
            color-mix(in srgb, ${bubbleColor} 50%, white) 50%,
            ${bubbleColor} 65%,
            ${bubbleColor} 100%
          );
          background-size: 200% auto;
          -webkit-background-clip: text;
          background-clip: text;
          -webkit-text-fill-color: transparent;
          animation: shimmer-sweep 2s ease-in-out infinite;
        }
      `}</style>
    </div>
  );
}
