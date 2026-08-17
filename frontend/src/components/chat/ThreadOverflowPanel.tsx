import { useEffect, useLayoutEffect, useRef, useState, type RefObject } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";
import { Trash2, X, Check, Inbox, ArchiveRestore } from "lucide-react";
import { tabIcon, tabLabel } from "./ThreadTabStrip";
import type { Thread } from "../../types/api";

/** Which row set the panel is currently browsing. Mirrors `ThreadsPanel`'s
 *  own active-list-vs-"Archived (N)"-section split, just as two peer tabs
 *  instead of a collapsed section — this panel has no sidebar real estate to
 *  spare, so archived threads get equal billing instead of a footnote. */
type PanelTab = "active" | "archived";

/** Matches the panel's own `w-[...]` below — kept as a constant instead of
 *  reading it back off the DOM because we need it to compute position
 *  BEFORE the (not-yet-positioned) panel has ever painted. */
const PANEL_WIDTH_PX = 280;
/** Gap between the "More" button and the panel's top edge. */
const ANCHOR_GAP_PX = 8;
/** Never let the panel touch the actual edge of the window — same
 *  clearance ChatInput's attach menu uses for the same reason. */
const VIEWPORT_MARGIN_PX = 8;

/** Right-aligns the panel under its anchor button, same as the old
 *  `right-0` CSS did, but clamped against BOTH edges of the window instead
 *  of just trusting there's room. Anchoring right-aligned by default is
 *  what keeps it tucked under the "More" pill (the strip's rightmost
 *  control) when there's space; the clamps are what stop it running behind
 *  the app sidebar when the pill sits close to the window's left edge
 *  instead (few threads open yet), or off the right edge on a narrow
 *  window. Mirrors `ChatInput`'s `openAttachMenu` positioning math. */
function computePosition(anchor: HTMLElement): { top: number; left: number } {
  const rect = anchor.getBoundingClientRect();
  let left = rect.right - PANEL_WIDTH_PX;
  if (left + PANEL_WIDTH_PX > window.innerWidth - VIEWPORT_MARGIN_PX) {
    left = window.innerWidth - PANEL_WIDTH_PX - VIEWPORT_MARGIN_PX;
  }
  if (left < VIEWPORT_MARGIN_PX) {
    left = VIEWPORT_MARGIN_PX;
  }
  return { top: rect.bottom + ANCHOR_GAP_PX, left };
}

interface ThreadOverflowPanelProps {
  open: boolean;
  onClose: () => void;
  /** The "More" pill's wrapper — used both to compute this panel's
   *  `position: fixed` coordinates (see `computePosition` above; the panel
   *  itself is portaled to `document.body`, so it has to know where its
   *  button actually is) and, in the outside-click listener below, to tell
   *  "clicked the toggle button again" apart from a genuine click
   *  elsewhere. */
  anchorRef: RefObject<HTMLElement | null>;
  /** Every non-default, non-archived thread — the same list `ThreadTabStrip`
   *  derives its pill row from. Shown on the "Active" tab. The default thread
   *  never appears: it's always pinned in the strip itself and can't be
   *  archived or deleted. */
  threads: Thread[];
  /** Every non-default, archived thread (see `archived_at` on `Thread`),
   *  newest-archived-first — shown on the "Archived" tab. Closing a pill
   *  (archiving) used to hide a thread from this panel entirely, reachable
   *  only via `ThreadsPanel`'s sidebar "Archived" section; this tab gives it
   *  a second, more discoverable home so a closed thread is never stranded
   *  behind a panel the user has to already know exists. */
  archivedThreads: Thread[];
  /** The thread currently shown in the conversation column — used to mark
   *  its row so the panel and the strip never disagree about what's open.
   *  Only ever matches a row on the "Active" tab — an archived thread can't
   *  be the active one. */
  activeThreadId: string;
  /** Switches the active thread — a row's label/icon click on the "Active"
   *  tab. Reuses the tab strip's own `onSelectThread`. Deliberately does NOT
   *  close this panel — the user asked to keep browsing/comparing threads
   *  after switching, not get kicked out on the first click. */
  onSelectThread: (threadId: string) => void;
  /** Deletes every given thread id, awaited in sequence. Reuses the tab
   *  strip's own `onDeleteThread` under the hood (via `ThreadTabStrip`'s
   *  `handleDeleteThreads`) so falling back off a deleted active thread and
   *  clearing its draft stay defined in exactly one place. Works the same
   *  regardless of which tab the selection came from — archived-or-not, a
   *  thread id deletes the same way. */
  onDeleteThreads: (threadIds: string[]) => Promise<void>;
  /** Restores an archived thread — fires on a click anywhere on the
   *  "Archived" tab's row (see the row rendering below; unlike
   *  `ThreadsPanel`'s sidebar, which uses a small separate restore icon,
   *  here the whole row IS the unarchive control). The caller (ChatView's
   *  `handleUnarchiveThread`) also switches to the thread once it's restored,
   *  so a click here both reopens and focuses it — the same one-click feel
   *  as clicking an Active tab row, not the sidebar's separate
   *  "unarchive, then go pick it" flow. This panel deliberately still does
   *  NOT close itself on that click, so the user can keep browsing/restoring
   *  more threads without round-tripping back through the "More" pill. */
  onUnarchiveThread: (threadId: string) => void | Promise<void>;
}

/** Floating "browse everything" panel for the tab strip's fixed "More" pill.
 *  Each row has two independent hit targets, mirroring how a pill itself
 *  splits into a select button and a close button: the checkbox toggles the
 *  row into a multi-delete set (never switches threads, never dismisses the
 *  panel — that's how picking several before deleting stays possible), while
 *  the label/icon is the row's primary action — switches the active thread
 *  on the "Active" tab, restores it on the "Archived" tab (also without
 *  dismissing, so the user can keep comparing/restoring threads from here
 *  instead of round-tripping back through the "More" pill). Dismissal is
 *  therefore deliberately narrow: an outside click, Escape, or the explicit
 *  close button — never a
 *  row interaction of either kind. */
export function ThreadOverflowPanel({
  open,
  onClose,
  anchorRef,
  threads,
  archivedThreads,
  activeThreadId,
  onSelectThread,
  onDeleteThreads,
  onUnarchiveThread,
}: ThreadOverflowPanelProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const [tab, setTab] = useState<PanelTab>("active");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [deleting, setDeleting] = useState(false);
  const [unarchivingId, setUnarchivingId] = useState<string | null>(null);
  // Viewport-relative coordinates for the portaled panel below — null until
  // the first open, then left holding its last value through the exit
  // animation (recomputed fresh on every subsequent open) rather than
  // resetting to null on close, so AnimatePresence's exit tween plays from
  // wherever the panel actually was instead of snapping away.
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);

  // Recomputed synchronously (before paint) on open, and again on resize —
  // this panel used to be an `absolute` child anchored under the "More"
  // button, but that nested it inside the chat column's `overflow-hidden`
  // "Right Panel" container (see AppShell), which silently clipped it
  // whenever the button sat close enough to the left edge that the panel's
  // right-aligned box ran past the sidebar boundary — reading as "the panel
  // is behind the sidebar" when it was actually just cut off. Portaling to
  // `document.body` with `position: fixed` (below) escapes that clipping
  // entirely, same fix `ChatInput`'s attach menu already uses for the same
  // overflow-hidden-ancestor problem.
  useLayoutEffect(() => {
    if (!open) return;
    const anchor = anchorRef.current;
    if (!anchor) return;
    const recompute = () => setPos(computePosition(anchor));
    recompute();
    window.addEventListener("resize", recompute);
    return () => window.removeEventListener("resize", recompute);
  }, [open, anchorRef]);

  // Fresh selection and tab on every open — a set left over from a previous
  // visit could otherwise point at threads deleted (or never seen) since,
  // and always starting back on "Active" matches what the old single-list
  // panel always showed first.
  useEffect(() => {
    if (open) {
      setSelected(new Set());
      setTab("active");
    }
  }, [open]);

  // Switching tabs clears the selection rather than carrying it across —
  // otherwise "N selected" could silently include rows from the tab you're
  // no longer looking at, and Delete would act on threads you can't see.
  // Belt-and-suspenders: the tab buttons only render at all when `selected`
  // is already empty (see the header below, which swaps to the "N
  // selected"/Clear/Delete row instead), so this never fires against a
  // non-empty set today — kept anyway so that invariant isn't required to
  // hold for this to stay correct if the header ever changes.
  function switchTab(next: PanelTab) {
    setTab(next);
    setSelected(new Set());
  }

  useEffect(() => {
    if (!open) return;
    const handleClick = (e: MouseEvent) => {
      if (
        panelRef.current && !panelRef.current.contains(e.target as Node) &&
        anchorRef.current && !anchorRef.current.contains(e.target as Node)
      ) {
        onClose();
      }
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKey);
    return () => {
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKey);
    };
  }, [open, onClose, anchorRef]);

  function toggle(threadId: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(threadId)) next.delete(threadId);
      else next.add(threadId);
      return next;
    });
  }

  async function handleDeleteSelected() {
    if (selected.size === 0 || deleting) return;
    setDeleting(true);
    try {
      await onDeleteThreads(Array.from(selected));
      setSelected(new Set());
    } finally {
      setDeleting(false);
    }
  }

  async function handleUnarchive(threadId: string) {
    setUnarchivingId(threadId);
    try {
      await onUnarchiveThread(threadId);
    } finally {
      setUnarchivingId(null);
    }
  }

  const visibleThreads = tab === "active" ? threads : archivedThreads;

  return createPortal(
    <AnimatePresence>
      {open && pos && (
        <motion.div
          ref={panelRef}
          role="dialog"
          aria-label="Threads"
          data-testid="thread-overflow-panel"
          // "Slides" down from the pill it hangs off of rather than just
          // fading in place — a small negative-y + scale start reads as the
          // panel unfurling from the button, not appearing out of nowhere.
          initial={{ opacity: 0, y: -8, scale: 0.97 }}
          animate={{ opacity: 1, y: 0, scale: 1 }}
          exit={{ opacity: 0, y: -8, scale: 0.97 }}
          transition={{ duration: 0.14, ease: "easeOut" }}
          // `fixed` + viewport coordinates from `pos` (computed above), not
          // `absolute top-full right-0` off the button anymore — see the
          // `useLayoutEffect` above for why: this panel is portaled straight
          // to `document.body` so it can float above EVERYTHING, including
          // the sidebar, instead of getting clipped by the chat column's
          // `overflow-hidden` ancestor.
          //
          // Border/shadow follow the same "elevated surface" recipe as our
          // other floating chrome (see SettingsModal etc.): a transparent
          // border in light mode with a hairline `0 0 0 1px` shadow ring
          // standing in for it (crisper than a flat CSS border), and a real
          // themed border in dark mode where a black ring would disappear
          // against the dark background. The blur/spread underneath is kept
          // light — a small anchored popover reads as cheap with a heavy
          // drop shadow; a soft, close shadow is what makes it feel modern.
          //
          // Uses the `--modal-*` namespace (not `--bg-secondary`/`--text-*`
          // directly) — same as CompetenciesModal/AssignmentEditorModal — so
          // this panel gets the forced-white surface those get in light mode
          // under a saturated "chrome" theme (Midnight/Sapphire/Emerald/Plum/
          // Denim/Goodstuff/Custom), instead of inheriting the theme's
          // colored --bg-secondary like the rest of the chat column does.
          className="fixed z-[60] w-[280px] max-h-[360px] flex flex-col
                   rounded-[14px] border border-transparent dark:border-[var(--modal-border-secondary)]
                   bg-[var(--modal-bg)] overflow-hidden"
          style={{
            top: pos.top,
            left: pos.left,
            transformOrigin: "top right",
            boxShadow: "0 0 0 1px rgba(0,0,0,0.1), 0 4px 12px rgba(0,0,0,0.08), 0 12px 28px rgba(0,0,0,0.12)",
          }}
        >
          <div className="flex items-center justify-between px-[14px] py-[12px] shrink-0">
            {selected.size > 0 ? (
              <>
                <span className="text-[12px] font-medium text-[var(--modal-text-secondary)]">
                  {selected.size} selected
                </span>
                <div className="flex items-center gap-[10px]">
                  <button
                    type="button"
                    onClick={() => setSelected(new Set())}
                    data-testid="thread-overflow-clear-selection"
                    className="text-[11px] text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)] cursor-pointer"
                  >
                    Clear
                  </button>
                  <button
                    type="button"
                    onClick={() => void handleDeleteSelected()}
                    disabled={deleting}
                    data-testid="thread-overflow-delete-selected"
                    className="inline-flex items-center gap-[4px] px-[8px] py-[4px] rounded-[6px] text-[11px] font-medium
                             bg-[var(--error)] text-white hover:opacity-90 disabled:opacity-50 cursor-pointer transition-opacity"
                  >
                    <Trash2 className="w-[11px] h-[11px]" />
                    {deleting ? "Deleting…" : "Delete"}
                  </button>
                </div>
              </>
            ) : (
              <>
                {/* Segmented tab switcher — replaces the old static "All
                    threads" title. `role="tablist"`/`role="tab"` here is
                    this panel's own pair, unrelated to (and not nested
                    inside) the strip's own `[role="tablist"]` of pills. */}
                <div role="tablist" aria-label="Thread visibility" className="flex items-center gap-[2px]">
                  <button
                    type="button"
                    role="tab"
                    aria-selected={tab === "active"}
                    onClick={() => switchTab("active")}
                    data-testid="thread-overflow-tab-active"
                    className={`px-[8px] py-[4px] rounded-[6px] text-[12px] font-medium cursor-pointer transition-colors ${
                      tab === "active"
                        ? "bg-[var(--modal-bg-hover)] text-[var(--modal-text-primary)]"
                        : "text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)]"
                    }`}
                  >
                    Active{threads.length > 0 ? ` (${threads.length})` : ""}
                  </button>
                  <button
                    type="button"
                    role="tab"
                    aria-selected={tab === "archived"}
                    onClick={() => switchTab("archived")}
                    data-testid="thread-overflow-tab-archived"
                    className={`px-[8px] py-[4px] rounded-[6px] text-[12px] font-medium cursor-pointer transition-colors ${
                      tab === "archived"
                        ? "bg-[var(--modal-bg-hover)] text-[var(--modal-text-primary)]"
                        : "text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)]"
                    }`}
                  >
                    Archived{archivedThreads.length > 0 ? ` (${archivedThreads.length})` : ""}
                  </button>
                </div>
                <button
                  type="button"
                  onClick={onClose}
                  data-testid="thread-overflow-close"
                  aria-label="Close"
                  className="w-[22px] h-[22px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)]
                           hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
                >
                  <X className="w-[13px] h-[13px]" />
                </button>
              </>
            )}
          </div>

          <div className="flex-1 min-h-0 overflow-y-auto overscroll-contain py-[4px]">
            {visibleThreads.length === 0 ? (
              <div className="flex flex-col items-center justify-center gap-[6px] py-[28px] text-[var(--modal-text-tertiary)] text-[12px]">
                <Inbox className="w-[20px] h-[20px] opacity-50" />
                {tab === "active" ? "No other threads yet" : "No archived threads"}
              </div>
            ) : (
              visibleThreads.map((thread) => {
                const isSelected = selected.has(thread.id);
                const isActive = tab === "active" && thread.id === activeThreadId;
                const Icon = tabIcon(thread);
                return (
                  <div
                    key={thread.id}
                    data-testid={tab === "archived" ? `thread-overflow-archived-row-${thread.id}` : undefined}
                    className={`group/row flex items-center gap-[8px] px-[12px] transition-colors ${
                      isSelected ? "bg-[var(--modal-bg-hover)]" : "hover:bg-[var(--modal-accent)]/12"
                    }`}
                  >
                    {/* Multi-delete checkbox — a sibling, not a child, of the
                        select button below (a button can't nest inside a
                        button); `stopPropagation` isn't even needed since
                        there's nothing to bubble into, but keeps this row
                        independent of the select button's own click. Shared
                        by both tabs — deleting works the same whether the
                        thread is archived or not. */}
                    <button
                      type="button"
                      onClick={() => toggle(thread.id)}
                      data-testid={`thread-overflow-checkbox-${thread.id}`}
                      aria-pressed={isSelected}
                      aria-label={isSelected ? "Deselect thread" : "Select thread"}
                      className={`shrink-0 w-[15px] h-[15px] rounded-[4px] border flex items-center justify-center transition-colors cursor-pointer ${
                        isSelected
                          ? "bg-[var(--modal-accent)] border-[var(--modal-accent)]"
                          : "border-[var(--modal-border-secondary)]"
                      }`}
                    >
                      {isSelected && <Check className="w-[10px] h-[10px] text-white" />}
                    </button>
                    {tab === "active" ? (
                      <button
                        type="button"
                        // Switches the active thread. Deliberately does NOT
                        // call onClose or toggle selection — this panel stays
                        // open so the user can keep browsing/switching (see
                        // the component doc comment above).
                        onClick={() => onSelectThread(thread.id)}
                        data-testid={`thread-overflow-row-${thread.id}`}
                        title={tabLabel(thread)}
                        className="flex-1 min-w-0 flex items-center gap-[8px] py-[7px] text-left cursor-pointer"
                      >
                        <Icon className="w-[13px] h-[13px] text-[var(--modal-text-tertiary)] shrink-0" />
                        <span className="flex-1 min-w-0 truncate text-[13px] text-[var(--modal-text-primary)]">
                          {tabLabel(thread)}
                        </span>
                        {isActive ? (
                          <span className="shrink-0 text-[10px] font-medium text-[var(--modal-accent)]">Current</span>
                        ) : null}
                      </button>
                    ) : (
                      // Archived rows: the whole row is the unarchive control
                      // now, not a separate small icon button off to the
                      // side — one click anywhere on the label restores it
                      // AND focuses it (switches to it, same as the Active
                      // tab's row), same "label/icon is the primary action"
                      // shape the Active tab's row already uses (there it's
                      // `onSelectThread`, here `onUnarchiveThread` chains into
                      // that same switch — see `handleUnarchiveThread` in
                      // ChatView). Label uses the same primary text colour as
                      // the Active tab's row (not a dimmer one) — an archived
                      // thread isn't a lesser thread, it's just tucked away.
                      // The colorful affordance lives on the row's hover
                      // background itself now (accent-tinted, same idea as
                      // the tab strip's `+` "new thread" button) — see the
                      // shared row `className` above — so the `ArchiveRestore`
                      // glyph stays a plain hover-reveal hint instead of
                      // carrying its own color too.
                      <button
                        type="button"
                        onClick={() => void handleUnarchive(thread.id)}
                        disabled={unarchivingId === thread.id}
                        data-testid={`thread-overflow-unarchive-${thread.id}`}
                        aria-label={`Unarchive ${tabLabel(thread)}`}
                        title="Click to unarchive"
                        className="flex-1 min-w-0 flex items-center gap-[8px] py-[7px] text-left cursor-pointer
                                 disabled:opacity-50 disabled:cursor-not-allowed"
                      >
                        <Icon className="w-[13px] h-[13px] text-[var(--modal-text-tertiary)] shrink-0" />
                        <span className="flex-1 min-w-0 truncate text-[13px] text-[var(--modal-text-primary)]">
                          {unarchivingId === thread.id ? "Restoring…" : tabLabel(thread)}
                        </span>
                        <span
                          className="shrink-0 w-[18px] h-[18px] flex items-center justify-center
                                   text-[var(--modal-text-tertiary)]
                                   opacity-0 group-hover/row:opacity-100 transition-opacity"
                        >
                          <ArchiveRestore className="w-[13px] h-[13px]" />
                        </span>
                      </button>
                    )}
                  </div>
                );
              })
            )}
          </div>
        </motion.div>
      )}
    </AnimatePresence>,
    document.body,
  );
}
