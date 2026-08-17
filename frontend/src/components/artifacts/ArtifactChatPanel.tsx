import { useCallback, useEffect, useRef, useState } from "react";
import { Loader2, X } from "lucide-react";
import * as api from "../../lib/api";
import { useDraftStore } from "../../stores/draftStore";
import { useArtifactChatTranscriptStore, type ArtifactChatMessage } from "../../stores/artifactChatTranscriptStore";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { useResolvedAgent } from "../../lib/agents/resolveAgent";
import { MessageBubble } from "../chat/MessageBubble";
import { ChatInput } from "../chat/ChatInput";
import type { TranscriptEntry } from "../../types/api";
import type { UseArtifactRegenResult } from "./useArtifactRegen";

/** Drag-to-resize clamps for the panel width — mirrors the clamp shape
 *  `ChatView.tsx`'s memory/bookmark/assets/todos side panel uses (240–800),
 *  narrowed on both ends since this panel sits inside an already-bounded
 *  artifact card rather than the full window: 260 keeps `ChatInput` usable,
 *  520 keeps the artifact body from being squeezed to nothing. */
const MIN_PANEL_WIDTH = 260;
const MAX_PANEL_WIDTH = 520;

export interface ArtifactChatPanelProps {
  agentId: string;
  artifactId: string;
  /** Shared with the header's Refresh button — same instance, so a chat send
   *  and a manual refresh can never race each other's polling, and the
   *  "Adjusting…" state this panel renders is exactly `useArtifactRegen`'s
   *  own `"working"` status. */
  regen: UseArtifactRegenResult;
  onClose: () => void;
}

/** Draft-store key for this artifact's chat mini-thread composer — mirrors
 *  the `team:{teamId}` convention `TeamsView` uses to scope a draft away
 *  from an agent's main chat draft (`stores/draftStore.ts`), so opening this
 *  panel never reads or clobbers whatever the user was mid-typing in the
 *  main Chat tab. Also doubles as this panel's `ChatInput` `conversationId`/
 *  `focusStoreKey`, so every per-conversation seam `ChatInput` exposes is
 *  scoped to this one artifact. */
export function artifactChatDraftKey(artifactId: string): string {
  return `artifact:${artifactId}`;
}

/** Turns one mini-thread entry into the same `TranscriptEntry` shape
 *  `MessageBubble` renders everywhere else in the app (Chat, Projects,
 *  Teams), so this panel gets identical bubble styling/avatars/timestamps
 *  for free. Deliberately does NOT go through `MessageList` or the shared
 *  `useChatStore`'s `selectedAgentId`/`messages` singleton: this panel is
 *  routinely opened as an overlay *on top of* an already-open, already-
 *  streaming main `ChatView` (see `MessageBubble.tsx`'s inline artifact-card
 *  click path, which opens `ArtifactPreview` while the chat behind it stays
 *  mounted) — repointing that singleton's active conversation to this
 *  artifact and back would race/clobber the live chat underneath. Rendering
 *  bare `MessageBubble`s off local state sidesteps that entirely, the same
 *  way `MessageList.tsx`'s `PinnedBookmarkOverlay`/`PinnedSearchOverlay`
 *  already render one synthetic entry each outside the virtualized list.
 *
 *  `agentId` is intentionally NOT passed through to `MessageBubble` (only
 *  `agentName`/`agentEmoji`, for the avatar): passing it would light up
 *  `MessageBubble`'s bookmark affordance, which assumes `entry.ts` is a real
 *  message timestamp in the agent's actual transcript — untrue here, these
 *  are chat-to-adjust turns, not part of the agent's conversation history. */
function toTranscriptEntry(m: ArtifactChatMessage, agentId: string, fallbackTs: string): TranscriptEntry {
  return {
    ts: m.ts ?? fallbackTs,
    role: m.role === "user" ? "user" : { agent: agentId },
    content: m.content,
    event_type: "message",
    // Only user turns show a status icon (`MessageBubble`'s `useMessageStatus`
    // no-ops for agent entries); fixed at "sent" since there's no real
    // messageId/seen-receipt plumbing for this mini-thread to key off of —
    // the alternative (falling through to the hook's "sending" default)
    // would leave every user bubble looking permanently unsent.
    status: m.role === "user" ? "sent" : undefined,
  };
}

/**
 * Scoped mini-thread for adjusting one artifact in place via chat (PRD
 * chat-to-adjust). Sending a message posts to `POST .../artifacts/{id}/chat`
 * then drives the shared `useArtifactRegen` instance exactly like the
 * header's Refresh button — "Adjusting…" while it works, and the artifact
 * body updates silently through `ArtifactPreview`'s existing refetch. The
 * agent's reply is never streamed synchronously (the endpoint is
 * fire-and-forget, see its doc comment); once the poll lands, this panel
 * fetches the artifact once more itself and shows the newest
 * `intent_ledger` entry's `intent_note` as the assistant's bubble.
 *
 * Composer and bubbles reuse the app's real `ChatInput`/`MessageBubble`
 * components (same as the Projects/Teams copilot overlays) for visual and
 * interaction parity — see `toTranscriptEntry` above for why that reuse
 * stops at the presentational layer rather than bridging into the shared
 * chat store the way `ProjectWorkspace`'s copilot does.
 */
export function ArtifactChatPanel({ agentId, artifactId, regen, onClose }: ArtifactChatPanelProps) {
  const draftKey = artifactChatDraftKey(artifactId);
  // Same `artifact:{artifactId}` key as the composer draft above, but in its
  // own store (a different value shape) — loaded once on mount so the
  // transcript survives navigating away from the Assets view and back within
  // this running session; mirrored back into the store on every change
  // below. The store is in-memory only (no localStorage) — the backend chat
  // transcript is the durable source of truth, hydrated below.
  const [messages, setMessages] = useState<ArtifactChatMessage[]>(
    () => useArtifactChatTranscriptStore.getState().transcripts[draftKey] ?? []
  );
  const setTranscript = useArtifactChatTranscriptStore((s) => s.setTranscript);
  const currentDraft = useDraftStore((s) => s.drafts[draftKey] ?? "");
  const currentDraftHtml = useDraftStore((s) => s.draftHtml[draftKey]);
  const setDraft = useDraftStore((s) => s.setDraft);
  const clearDraft = useDraftStore((s) => s.clearDraft);
  const listRef = useRef<HTMLDivElement>(null);

  // Drag-to-resize width — persisted per the app (not per-artifact) via
  // `userPreferencesStore`, same as `ChatView`'s side panel. Width is
  // mutated directly on the DOM node while dragging (no re-render per
  // mousemove) and only committed to the store on mouseup, mirroring
  // `ChatView.tsx`'s `startResizing`/`resize`/`stopResizing` trio.
  const panelWidth = useUserPreferencesStore((s) => s.artifactChatPanelWidth);
  const setPanelWidth = useUserPreferencesStore((s) => s.setArtifactChatPanelWidth);
  const panelRef = useRef<HTMLDivElement>(null);
  const resizingRef = useRef(false);
  const [isResizing, setIsResizing] = useState(false);

  const startResizing = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    resizingRef.current = true;
    setIsResizing(true);
  }, []);

  const stopResizing = useCallback(() => {
    if (resizingRef.current && panelRef.current) {
      setPanelWidth(panelRef.current.offsetWidth);
    }
    resizingRef.current = false;
    setIsResizing(false);
  }, [setPanelWidth]);

  const resize = useCallback((e: MouseEvent) => {
    if (!resizingRef.current || !panelRef.current) return;
    // The handle sits on the panel's left edge and the panel itself is
    // anchored to the right (border-l, flex sibling of the artifact body),
    // so dragging left (smaller clientX) should widen it — same inverted
    // relationship `ChatView.tsx`'s `resize` uses for its right-side panel.
    const newWidth = panelRef.current.getBoundingClientRect().right - e.clientX;
    if (newWidth >= MIN_PANEL_WIDTH && newWidth <= MAX_PANEL_WIDTH) {
      panelRef.current.style.width = `${newWidth}px`;
    }
  }, []);

  useEffect(() => {
    if (isResizing) {
      window.addEventListener("mousemove", resize);
      window.addEventListener("mouseup", stopResizing);
      document.body.style.cursor = "col-resize";
    } else {
      window.removeEventListener("mousemove", resize);
      window.removeEventListener("mouseup", stopResizing);
      document.body.style.cursor = "default";
    }
    return () => {
      window.removeEventListener("mousemove", resize);
      window.removeEventListener("mouseup", stopResizing);
      document.body.style.cursor = "default";
    };
  }, [isResizing, resize, stopResizing]);

  // Read-only lookup against the agent roster (`useChatStore`'s `agents`
  // list) for the avatar — the same "harmless, never writes" read `ChatInput`
  // already does for `selectedAgentProfile`. Never touches `selectedAgentId`
  // or `messages`, so it can't race the live chat behind this overlay.
  const resolved = useResolvedAgent(agentId);

  useEffect(() => {
    setTranscript(draftKey, messages);
  }, [draftKey, messages, setTranscript]);

  // Seeds the in-memory store from the durable server transcript on mount /
  // artifact change — the server is now the sole durable source of truth,
  // there's no competing persisted local copy to reconcile against. The one
  // thing still worth guarding: this fetch can resolve after the user has
  // already sent a message in the same instant (awaitingReplyRef flips
  // before this promise settles), so any not-yet-confirmed entry already in
  // state is kept and appended after the server's copy rather than being
  // clobbered by a stale response. A failed fetch (including in tests that
  // don't mock `getArtifactChat`) is swallowed — the panel just keeps
  // showing whatever's already in memory rather than crashing or clearing
  // it.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      let chat: Awaited<ReturnType<typeof api.getArtifactChat>>;
      try {
        chat = await api.getArtifactChat(agentId, artifactId);
      } catch {
        return;
      }
      if (cancelled) return;
      const serverMessages: ArtifactChatMessage[] = chat.entries
        .filter((entry) => !entry.hidden_from_user)
        .map((entry) => ({ role: entry.role, content: entry.content, ts: entry.ts }));
      setMessages((prev) => {
        // `remaining` tracks how many more times each (role, content) pair
        // still needs to show up in `prev` before it's "confirmed" by the
        // server copy. Each local entry consumes one matching count if
        // available (dropped — the server already has it); anything left
        // over is a bubble the server hasn't caught up to yet, kept and
        // appended after the server's copy.
        const remaining = new Map<string, number>();
        for (const entry of serverMessages) {
          const key = `${entry.role} ${entry.content}`;
          remaining.set(key, (remaining.get(key) ?? 0) + 1);
        }
        const unconfirmed = prev.filter((entry) => {
          const key = `${entry.role} ${entry.content}`;
          const count = remaining.get(key) ?? 0;
          if (count > 0) {
            remaining.set(key, count - 1);
            return false;
          }
          return true;
        });
        return [...serverMessages, ...unconfirmed];
      });
    })();
    return () => {
      cancelled = true;
    };
  }, [agentId, artifactId]);
  // True from the moment a chat send kicks off `regen.start()` until that
  // run's completion (or failure) has been turned into a reply bubble. Lets
  // the completion effect below tell "this working->done was my chat turn"
  // apart from "the user clicked Refresh while the panel happened to be
  // open" — both drive the same shared `regen` instance.
  const awaitingReplyRef = useRef(false);

  useEffect(() => {
    const el = listRef.current;
    if (!el) return;
    // Plain scrollTop assignment, not `scrollTo` — jsdom (used in tests)
    // doesn't implement `Element.scrollTo`, and this doesn't need smooth
    // scrolling anyway.
    el.scrollTop = el.scrollHeight;
  }, [messages]);

  useEffect(() => {
    if (!awaitingReplyRef.current) return;
    if (regen.status === "done") {
      awaitingReplyRef.current = false;
      api
        .getArtifact(agentId, artifactId)
        .then((updated) => {
          const ledger = updated.intent_ledger ?? [];
          const last = ledger[ledger.length - 1];
          const note = last?.intent_note?.trim();
          setMessages((prev) => [
            ...prev,
            { role: "assistant", content: note || "Done.", ts: last?.timestamp ?? new Date().toISOString() },
          ]);
        })
        .catch((err) => {
          setMessages((prev) => [
            ...prev,
            {
              role: "assistant",
              content: `Adjustment applied, but I couldn't load my reply (${(err as Error).message}).`,
              ts: new Date().toISOString(),
            },
          ]);
        });
    } else if (regen.status === "error") {
      awaitingReplyRef.current = false;
      setMessages((prev) => [
        ...prev,
        {
          role: "assistant",
          content: `Sorry, that didn't work: ${regen.error ?? "unknown error"}`,
          ts: new Date().toISOString(),
        },
      ]);
    }
  }, [regen.status, regen.error, agentId, artifactId]);

  const busy = regen.status === "working";

  const handleSend = useCallback(
    async (text: string) => {
      const trimmed = text.trim();
      if (!trimmed || busy) return;
      const transcript: api.ChatTranscriptTurn[] = messages.map((m) => ({ role: m.role, content: m.content }));
      setMessages((prev) => [...prev, { role: "user", content: trimmed, ts: new Date().toISOString() }]);
      clearDraft(draftKey);
      awaitingReplyRef.current = true;
      await regen.start(() => api.chatArtifact(agentId, artifactId, trimmed, transcript));
    },
    [busy, messages, clearDraft, draftKey, regen, agentId, artifactId]
  );

  return (
    <div
      ref={panelRef}
      data-testid="artifact-chat-panel"
      data-artifact-id={artifactId}
      className="relative shrink-0 border-l flex flex-col min-h-0"
      style={{ borderColor: "var(--border-primary)", width: panelWidth }}
    >
      {/* Resizer handle — same shape as `ChatView.tsx`'s side-panel divider:
         an oversized invisible drag target with a thin accent line that only
         shows on hover/drag, so the hit area is comfortable without a
         visually thick border. */}
      <div
        className="absolute left-[-4px] top-0 bottom-0 w-[8px] cursor-col-resize z-30 group"
        onMouseDown={startResizing}
      >
        <div className="absolute inset-y-0 left-1/2 w-[1px] bg-transparent group-hover:bg-[var(--accent)] transition-colors" />
      </div>

      {/* Drag-capture overlay — `ChatView.tsx`'s panel never sits beside an
         iframe, but this one does for HTML-kind artifacts (`HtmlArtifactBody`).
         An iframe is a separate document: once the cursor crosses onto it
         mid-drag, `mousemove` is captured by the iframe's own document and
         never reaches this component's `window` listener, so the drag stalls
         dead the instant it crosses the artifact body. `position: fixed`
         (not `absolute`) so it covers the whole card regardless of where the
         panel sits in the row, at a z-index above both the iframe and the
         resize handle so it — not the iframe — receives the pointer for the
         whole gesture. */}
      {isResizing && <div className="fixed inset-0 z-50 cursor-col-resize" />}

      <div
        className="px-3 py-2 flex items-center justify-between border-b shrink-0"
        style={{ borderColor: "var(--border-primary)" }}
      >
        <span className="text-[12px] font-semibold" style={{ color: "var(--text-primary)" }}>
          Adjust with chat
        </span>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close chat panel"
          className="w-[22px] h-[22px] rounded-[6px] flex items-center justify-center hover:bg-[var(--bg-hover)]"
          style={{ color: "var(--text-secondary)" }}
        >
          <X size={12} />
        </button>
      </div>

      <div ref={listRef} className="flex-1 min-h-0 overflow-y-auto px-2 py-2 flex flex-col gap-1 custom-scrollbar">
        {messages.length === 0 && (
          <p className="text-[12px] px-1" style={{ color: "var(--text-secondary)" }}>
            Ask for a change and it will be applied to this artifact in place.
          </p>
        )}
        {messages.map((m, i) => (
          <div key={i} data-testid={`artifact-chat-message-${m.role}`}>
            <MessageBubble
              entry={toTranscriptEntry(m, agentId, m.ts ?? new Date(0).toISOString())}
              agentName={resolved.displayName}
              agentEmoji={resolved.emoji}
            />
          </div>
        ))}
        {busy && (
          <div
            data-testid="artifact-chat-adjusting"
            className="flex items-center gap-1.5 text-[12px] px-1"
            style={{ color: "var(--text-secondary)" }}
          >
            <Loader2 size={12} className="animate-spin" /> Adjusting…
          </div>
        )}
      </div>

      <div className="p-2 shrink-0">
        <ChatInput
          onSend={(text) => void handleSend(text)}
          disabled={busy}
          placeholder="Describe the change…"
          conversationId={draftKey}
          focusStoreKey={draftKey}
          initialDraft={currentDraft}
          initialDraftHtml={currentDraftHtml}
          onUnmount={(text, html, id) => {
            if (id && text.trim()) setDraft(id, text, html);
            else if (id) clearDraft(id);
          }}
        />
      </div>
    </div>
  );
}
