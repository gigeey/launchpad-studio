import { motion } from "framer-motion";
import {
  agentIdFromInFlightKey,
  hasPendingSyncFormForThread,
  inFlightKey,
  pendingFormForThread,
  type InFlightAgentMessage,
  type RunningDelegateInfo,
} from "../../stores/chatStore";
import type { PendingForm, Thread } from "../../types/api";
import type { FormRequestPayload } from "../../types/form";

/** Four-state activity a thread — or, aggregated, an entire agent — can be
 *  in from the operator's point of view: waiting on an answer to a form it
 *  posted, actively producing output right now, done producing output the
 *  operator hasn't looked at yet, or neither. Shared by Chat's
 *  `ThreadTabStrip` and Home's `HomeSidebar` so "this is running" / "this
 *  has something new" always reads identically no matter which nav surface
 *  it's seen from. */
export type ThreadActivity = "question" | "streaming" | "unread" | "none";

/** Composes the same composite key `inFlightByAgent`/`unreadThreadIds` use —
 *  the plain agent id for the default thread, `inFlightKey(agentId, id)`
 *  otherwise (mirrors the backend's `AgentEvent.thread_id` tagging, which is
 *  omitted for the default thread and present for everything else). Keeping
 *  this in one place means every surface that reads thread activity agrees
 *  on how the store tags default-vs-non-default threads. */
export function threadActivityKey(agentId: string, thread: Pick<Thread, "id" | "kind">): string {
  return thread.kind === "default" ? agentId : inFlightKey(agentId, thread.id);
}

/** A thread counts as "streaming" the instant its run starts producing
 *  anything observable — the typing indicator, the first character of text,
 *  or the first tool-call chip — not just while raw text is actively
 *  arriving. */
export function isThreadStreaming(entry: InFlightAgentMessage | undefined): boolean {
  return !!entry && (entry.isTyping || entry.textBuffer.length > 0 || entry.activeToolCalls.length > 0);
}

/** One thread's activity, read straight from the chatStore maps that drive
 *  it. Priority, highest first: `question` (the thread is blocked on an
 *  unanswered form — either a sync `AskUserQuestionWithForm` call, checked
 *  via `hasPendingSyncFormForThread`, or an async form in the agent's own
 *  `pending_forms`, checked via `pendingFormForThread` — a form only leaves
 *  `pending_forms` once it's answered, dismissed, or superseded, so simple
 *  presence is the whole "is this still an open question" check) outranks
 *  `streaming`, which outranks `unread`. A thread can be both actively
 *  streaming and waiting on a form at once (the run that posted the form is often still
 *  "active" from the backend's point of view); `question` wins because it's
 *  the one state that needs the operator to act, whereas a stale unread dot
 *  is comparatively passive. `runningDelegatesByThread`, `pendingFormByAgent`,
 *  and `pendingForms` are all optional (defaulting to "none pending" / "none
 *  running") so existing callers that haven't been taught about async
 *  Delegate activity or form state yet keep compiling/behaving exactly as
 *  before — a thread with a running background delegate but no active LLM
 *  turn also counts as `"streaming"`, since from the operator's point of view
 *  both mean "something is happening here right now". */
export function resolveThreadActivity(
  agentId: string,
  thread: Thread,
  inFlightByAgent: Map<string, InFlightAgentMessage>,
  unreadThreadIds: Set<string>,
  runningDelegatesByThread?: Map<string, Map<string, RunningDelegateInfo>>,
  pendingFormByAgent?: Record<string, FormRequestPayload | undefined>,
  pendingForms?: PendingForm[],
): ThreadActivity {
  const key = threadActivityKey(agentId, thread);
  const threadId = thread.kind === "default" ? undefined : thread.id;
  // Derived straight off the `pendingForms` snapshot prop every caller of
  // this function already passes — no transcript lookup needed (this
  // resolves activity for every thread row, including background/collapsed
  // ones with no transcript loaded client-side). Presence in `pendingForms`
  // for this thread already means "still an open question" — see
  // `pendingFormForThread`'s docstring.
  const hasUnansweredForm =
    (pendingFormByAgent != null && hasPendingSyncFormForThread(pendingFormByAgent, agentId, threadId)) ||
    pendingFormForThread(pendingForms, threadId) != null;
  if (hasUnansweredForm) return "question";
  if (isThreadStreaming(inFlightByAgent.get(key))) return "streaming";
  if ((runningDelegatesByThread?.get(key)?.size ?? 0) > 0) return "streaming";
  return unreadThreadIds.has(key) ? "unread" : "none";
}

/** Whether `thread`'s pending "question" activity (see `resolveThreadActivity`
 *  above) comes from a SYNC `AskUserQuestionWithForm` call rather than an
 *  async `pending_forms` entry — sync forms block the agent's run until
 *  answered, so render sites use this to switch `ThreadQuestionBadge` into
 *  its louder `sync` treatment. Only meaningful when that thread's activity
 *  is actually `"question"`; harmless (just `false`) otherwise. Mirrors
 *  `resolveThreadActivity`'s own default-thread key derivation so the two
 *  never disagree about which bucket a thread's form falls into. */
export function isSyncQuestion(
  agentId: string,
  thread: Pick<Thread, "id" | "kind">,
  pendingFormByAgent: Record<string, FormRequestPayload | undefined>,
): boolean {
  const threadId = thread.kind === "default" ? undefined : thread.id;
  return hasPendingSyncFormForThread(pendingFormByAgent, agentId, threadId);
}

/** Aggregates every thread's activity up to one flag per agent, for surfaces
 *  (Home's collapsed agent rows) that need "does this agent have anything
 *  going on right now" at a glance, without that agent's thread list having
 *  been fetched yet — `agentIdFromInFlightKey` recovers the owning agent
 *  from a composite key directly, so this works even before `threadsByAgent`
 *  has loaded. Streaming wins over unread, same priority as a single
 *  thread's own activity above. `runningDelegatesByThread` is optional, same
 *  back-compat reasoning as `resolveThreadActivity`. */
export function resolveAgentActivityMap(
  inFlightByAgent: Map<string, InFlightAgentMessage>,
  unreadThreadIds: Set<string>,
  runningDelegatesByThread?: Map<string, Map<string, RunningDelegateInfo>>,
): Map<string, ThreadActivity> {
  const map = new Map<string, ThreadActivity>();
  for (const [key, entry] of inFlightByAgent) {
    if (!isThreadStreaming(entry)) continue;
    map.set(agentIdFromInFlightKey(key), "streaming");
  }
  for (const [key, delegates] of runningDelegatesByThread ?? []) {
    if (delegates.size > 0) map.set(agentIdFromInFlightKey(key), "streaming");
  }
  for (const key of unreadThreadIds) {
    const agentId = agentIdFromInFlightKey(key);
    if (map.get(agentId) !== "streaming") map.set(agentId, "unread");
  }
  return map;
}

/** Google-style palette for the streaming badge — literal hex values, not
 *  `var(--...)` theme tokens. Framer Motion can't tween a CSS custom
 *  property (it doesn't resolve `var(--accent)` to a color before
 *  interpolating), so an earlier version of this badge silently never
 *  animated its color at all. Plain hex/rgb values tween correctly. */
const STREAMING_BADGE_COLORS = ["#EA4335", "#4285F4", "#FBBC05"]; // red, blue, yellow
const [BADGE_RED, BADGE_BLUE, BADGE_YELLOW] = STREAMING_BADGE_COLORS;

/** Sync question badge's fill cycle — the same `STREAMING_BADGE_COLORS`
 *  palette, looped back to its own first entry so the repeat lands with no
 *  jump-cut. One entry advances per `animate-ping` cycle (Tailwind's
 *  `animate-ping` period is a fixed 1s), so `duration` below is deliberately
 *  `STREAMING_BADGE_COLORS.length` seconds — one second per color. */
const SYNC_QUESTION_BADGE_COLOR_VALUES = [...STREAMING_BADGE_COLORS, STREAMING_BADGE_COLORS[0]];
const SYNC_QUESTION_BADGE_CYCLE_DURATION = STREAMING_BADGE_COLORS.length;

/** One full color cycle, in seconds. Slower than an original 1.4s attempt —
 *  that read as a fast flip because the color only actually moved during
 *  brief ~70ms windows sandwiched between long static holds. Stretching both
 *  the cycle and the transition windows below (see `REST_END`→`LANDED`) is
 *  what turns it into smooth, visible motion instead of a snap. */
const BREATH_DURATION = 2.6;
/** How large the badge swells at the peak of each breath, relative to its
 *  resting scale of 1. */
const BREATH_PEAK_SCALE = 1.45;

/** The loop is divided into 3 equal segments, one per color. Each segment
 *  rests (steady shape/color/scale) for its first 55%, then breathes: scale
 *  swells to `BREATH_PEAK_SCALE` at the segment's 77.5% mark while its
 *  shape/color eases toward the next value, then scale settles back to 1
 *  right as that new shape/color fully lands at the segment boundary — which
 *  doubles as the next segment's rest value. Nothing ever holds still and
 *  then jump-cuts; it reads as one continuous in/out breath per color. */
const SEGMENT = 1 / 3;
const REST_END = [0, 1, 2].map((i) => i * SEGMENT + SEGMENT * 0.55);
const PEAK = [0, 1, 2].map((i) => i * SEGMENT + SEGMENT * 0.775);
const LANDED = [0, 1, 2].map((i) => (i + 1) * SEGMENT);

/** Shape, color, and (below) scale all share this exact tick schedule — one
 *  transition per color, each spanning a segment's `REST_END` → `LANDED`
 *  window, holding still everywhere else. Sharing one array means every
 *  property that moves, moves at the exact same instant. Landing on
 *  `LANDED[2]` reproduces `t=0`'s values exactly, so the loop repeats with
 *  no jump-cut. */
const TICK_TIMES = [0, REST_END[0], LANDED[0], REST_END[1], LANDED[1], REST_END[2], LANDED[2]];

/** Shape cycles through three distinct rounded-square states — circle, sharp
 *  square, and an in-between "squoval" — one per color, so it always has a
 *  transition to pair with every color change. Rotation advances a steady
 *  120° per transition so it still reads as one continuous 360° spin by the
 *  time the loop closes back to its starting shape. */
const SHAPE_VALUES = ["50%", "50%", "20%", "20%", "35%", "35%", "50%"];
const ROTATE_VALUES = [0, 0, 120, 120, 240, 240, 360];

/** Color eases from old to new across each segment's breathe window
 *  (`REST_END` → `LANDED`) — the exact same windows shape transitions
 *  through above — landing on the new color right as the badge settles back
 *  down from its scale peak. */
const COLOR_VALUES = [BADGE_RED, BADGE_RED, BADGE_BLUE, BADGE_BLUE, BADGE_YELLOW, BADGE_YELLOW, BADGE_RED];

/** Scale breathes in and back out across each segment's transition window,
 *  peaking at `PEAK` — exactly midway between `REST_END` and `LANDED`. */
const SCALE_TIMES = [
  0,
  REST_END[0], PEAK[0], LANDED[0],
  REST_END[1], PEAK[1], LANDED[1],
  REST_END[2], PEAK[2], LANDED[2],
];
const SCALE_VALUES = [1, 1, BREATH_PEAK_SCALE, 1, 1, BREATH_PEAK_SCALE, 1, 1, BREATH_PEAK_SCALE, 1];

/** Small "alive" badge for a row whose thread (or, aggregated, one of an
 *  agent's threads) is actively streaming right now. Rotates while morphing
 *  through three rounded shapes in lockstep with three colors
 *  (red/blue/yellow), scaling up and back down at each shape/color change —
 *  a continuous breathing pulse rather than a static dot or a fast flip —
 *  distinct from the (static) unread-after-the-fact indicator below. `id`
 *  only feeds the testid, so callers can pass a thread id or an agent id
 *  depending on what's being badged. */
export function ThreadStreamingBadge({ id }: { id: string }) {
  return (
    <motion.span
      aria-hidden
      data-testid={`thread-streaming-badge-${id}`}
      className="block w-[7px] h-[7px]"
      style={{ backgroundColor: BADGE_RED }}
      animate={{
        borderRadius: SHAPE_VALUES,
        rotate: ROTATE_VALUES,
        backgroundColor: COLOR_VALUES,
        scale: SCALE_VALUES,
      }}
      transition={{
        default: {
          duration: BREATH_DURATION,
          repeat: Infinity,
          ease: "easeInOut",
          times: TICK_TIMES,
        },
        backgroundColor: {
          duration: BREATH_DURATION,
          repeat: Infinity,
          ease: "easeInOut",
          times: TICK_TIMES,
        },
        scale: {
          duration: BREATH_DURATION,
          repeat: Infinity,
          ease: "easeInOut",
          times: SCALE_TIMES,
        },
      }}
    />
  );
}

/** Static accent dot — a thread (or, aggregated, one of an agent's threads)
 *  finished streaming while it wasn't being looked at and hasn't been opened
 *  since. Disappears the moment the user selects/opens it (see
 *  `markThreadViewed`). `id` only feeds the testid, same convention as
 *  `ThreadStreamingBadge` above. */
export function ThreadUnreadDot({ id }: { id: string }) {
  return (
    <span
      aria-hidden
      data-testid={`thread-unread-dot-${id}`}
      className="block w-[6px] h-[6px] rounded-full bg-[var(--unread-badge-bg,var(--accent))]"
    />
  );
}

/** "?" glyph shared by both `ThreadQuestionBadge` variants — a bare text
 *  glyph rather than an enclosed icon shape, so it reads as a naked question
 *  mark on top of the badge's own circular container instead of a second,
 *  smaller ring nested inside it. Sized to roughly 75% of the 16px badge
 *  diameter so it nearly fills the circle. */
function QuestionGlyph() {
  return (
    <span aria-hidden style={{ color: "white", fontWeight: 700, lineHeight: 1, fontSize: 12 }}>
      ?
    </span>
  );
}

/** "Needs an answer" badge — a thread (or, aggregated, one of an agent's
 *  threads) is blocked on an unanswered form, either a sync
 *  `AskUserQuestionWithForm` call or an async entry in `pending_forms`.
 *  Outranks both `ThreadStreamingBadge` and `ThreadUnreadDot` (see
 *  `resolveThreadActivity`'s priority order) since it's the one state that
 *  needs the operator to actually do something, not just notice it. Fills
 *  a 16x16 icon slot, since a bare dot can't carry a legible glyph at that
 *  size. Renders `QuestionGlyph` centered inside. `id` only feeds the
 *  testid, same convention as the other badges above.
 *
 *  `sync` (see `isSyncQuestion`) switches to a louder badge with a pinging
 *  ring — the same `animate-ping` treatment `OwnerAvatar` uses for "this
 *  agent is actively working right now" (an absolutely-positioned ring the
 *  size of the badge, expanding/fading via Tailwind's `animate-ping`
 *  keyframes) — for a blocking `AskUserQuestionWithForm` call, since the
 *  agent's run is stalled until it's answered. Its fill cycles through
 *  `STREAMING_BADGE_COLORS` (see `SYNC_QUESTION_BADGE_COLOR_VALUES` below),
 *  one color per ping, so the badge keeps announcing "still stalled" for as
 *  long as the form goes unanswered. An async `pending_forms` entry keeps
 *  running regardless and stays fully static here, no ring, no color cycle.
 *  The glyph, testid, and layout stay identical either way; only
 *  color/motion distinguish the two. */
export function ThreadQuestionBadge({ id, sync = false }: { id: string; sync?: boolean }) {
  if (!sync) {
    return (
      <span
        aria-hidden
        data-testid={`thread-question-badge-${id}`}
        className="flex items-center justify-center w-[16px] h-[16px] rounded-full bg-amber-500 text-white"
      >
        <QuestionGlyph />
      </span>
    );
  }
  return (
    <motion.span
      aria-hidden
      data-testid={`thread-question-badge-${id}`}
      data-sync="true"
      className="relative flex items-center justify-center w-[16px] h-[16px] rounded-full text-white"
      style={{ backgroundColor: BADGE_RED }}
      animate={{ backgroundColor: SYNC_QUESTION_BADGE_COLOR_VALUES }}
      transition={{ duration: SYNC_QUESTION_BADGE_CYCLE_DURATION, repeat: Infinity, ease: "linear" }}
    >
      <QuestionGlyph />
      <span
        className="absolute inset-0 rounded-full animate-ping pointer-events-none"
        style={{ boxShadow: "0 0 0 2px rgba(220,38,38,0.45)" }}
        aria-hidden
      />
    </motion.span>
  );
}
