import { lazy, memo, Suspense, useEffect, useMemo, useState, type ReactElement } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import rehypeKatex from "rehype-katex";
import "katex/dist/katex.min.css";

import { Bookmark, Box, Check, ChevronDown, Code2, Copy, ExternalLink, GitBranch, ImageOff, Loader2, RotateCw, Sparkles } from "lucide-react";
import { TranscriptEntry, isUserMessage, Attachment, ArtifactKind, RefreshIntent } from "../../types/api";
import { useChatStore } from "../../stores/chatStore";
import { branchFromMessage } from "../../lib/threadNavigation";
import { useAgentProfileModalStore } from "../../stores/agentProfileModalStore";
import { useBookmarkStore } from "../../stores/bookmarkStore";
import { useAttachmentStore, AttachmentFetcher } from "../../stores/attachmentStore";
import { useMediaPreviewStore } from "../../stores/mediaPreviewStore";
import { useArtifactStore } from "../../stores/artifactStore";
import { ArtifactPreview } from "../artifacts/ArtifactRenderer";
import { openArtifactWindow } from "../../lib/windows";
import { StatusIndicator } from "./StatusIndicator";
import { agentAvatarColor } from "../../lib/agentColors";
import { adjustBubbleColor } from "../../lib/colorUtils";
import { getAttachmentUrl } from "../../lib/api";
import { truncateFilename } from "./AttachmentPill";
import { FileIcon } from "./FileIcon";
import { useIsDark, useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { escapeRawHtmlOutsideCode } from "../../lib/escapeRawHtml";

const LazyMermaidBlock = lazy(() =>
  import("./MermaidBlock").then((mod) => ({ default: mod.MermaidBlock }))
);

/** Rehype plugin: strip stray backtick characters from text nodes.
 *  After remark parses the markdown, any backticks that formed valid code spans
 *  or fenced blocks are already consumed. Remaining backticks in text nodes
 *  are strays from mismatched pairs — safe to remove. */
function rehypeStripBackticks() {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  return (tree: any) => {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    function walk(node: any) {
      if (node.type === 'text' && typeof node.value === 'string' && node.value.includes('`')) {
        node.value = node.value.replace(/`/g, '');
      }
      if (node.children) {
        node.children.forEach(walk);
      }
    }
    walk(tree);
  };
}

// Known agent-emitted action tags. Kept in sync with the backend
// tag_stream_scanner registry — their presence in content must not count as
// visible text when deciding whether a bubble has anything to render.
//
// `tool_use` / `tool_result` (the continuation wire format) are included so
// the raw XML the model emits in its `response` event (e.g. "Let me try it
// now.\n\n<tool_use ...>…</tool_use>") doesn't leak into the displayed
// bubble. The dispatch
// chip + tool_result transcript entries are the user-facing surfaces; the
// inline XML is plumbing.
const AGENT_ACTION_TAG_NAMES = "save_memory|save_global_memory|scheduled_task|write_skill|recall_history|workflow_task|load_skill|tool_use|tool_result";
const AGENT_ACTION_PAIRED_RE = new RegExp(`<(${AGENT_ACTION_TAG_NAMES})(?:\\s[^>]*)?>[\\s\\S]*?</\\1>`, "g");
const AGENT_ACTION_LOOSE_RE = new RegExp(`</?(?:${AGENT_ACTION_TAG_NAMES})(?:\\s[^>]*)?/?>`, "g");

/** Strip agent-action tags (paired, self-closing, or orphan) from text and
 *  return the remaining content. Used to decide whether a bubble has anything
 *  worth rendering. */
export function stripAgentActionTags(text: string): string {
  return text.replace(AGENT_ACTION_PAIRED_RE, "").replace(AGENT_ACTION_LOOSE_RE, "");
}

type MentionSegment =
  | { type: "text"; value: string }
  | { type: "mention"; id: string; name: string };

const MENTION_RE = /<mention\s+id="([^"]*)">(.*?)<\/mention>/g;

/** Split a string on `<mention id="...">name</mention>` tags. */
function parseMentionSegments(text: string): MentionSegment[] {
  const segments: MentionSegment[] = [];
  let lastIndex = 0;
  let match: RegExpExecArray | null;

  while ((match = MENTION_RE.exec(text)) !== null) {
    if (match.index > lastIndex) {
      segments.push({ type: "text", value: text.slice(lastIndex, match.index) });
    }
    segments.push({ type: "mention", id: match[1], name: match[2] });
    lastIndex = match.index + match[0].length;
  }

  if (lastIndex < text.length) {
    segments.push({ type: "text", value: text.slice(lastIndex) });
  }

  // Reset lastIndex for the global regex
  MENTION_RE.lastIndex = 0;

  return segments;
}

/** Clickable bold mention badge. Links to agent profile; gracefully handles missing agents. */
function MentionTag({ id, name, onAccent }: { id: string; name: string; onAccent?: boolean }) {
  return (
    <span
      role="link"
      tabIndex={0}
      onClick={() => useAgentProfileModalStore.getState().openEdit(id)}
      onKeyDown={(e) => e.key === "Enter" && useAgentProfileModalStore.getState().openEdit(id)}
      className={`font-bold cursor-pointer hover:underline ${onAccent ? "text-white/90 hover:text-white" : "text-[var(--accent)]"}`}
    >
      @{name}
    </span>
  );
}

const remarkPluginsMath = [remarkGfm, remarkMath];
const rehypePluginsMath = [rehypeStripBackticks, rehypeKatex];
const remarkPluginsPlain = [remarkGfm];
const rehypePluginsPlain = [rehypeStripBackticks];

/** Check if content likely contains real LaTeX math (not just currency $). */
function hasLatexMath(text: string): boolean {
  // Display math: $$...$$
  if (text.includes("$$")) return true;
  // Inline math with LaTeX commands: $\cmd
  if (/\$\\[a-zA-Z]/.test(text)) return true;
  // Inline math with braces: ${...}
  if (/\$\{/.test(text)) return true;
  return false;
}
const mdComponents = {
  table({ children }: { children?: React.ReactNode }) {
    return (
      <div className="overflow-x-auto">
        <table>{children}</table>
      </div>
    );
  },
  code({ className, children }: { className?: string; children?: React.ReactNode }) {
    const match = /language-mermaid/.exec(className || "");
    if (match) {
      return (
        <Suspense fallback={<div className="my-2 text-xs text-gray-400">Loading diagram…</div>}>
          <LazyMermaidBlock code={String(children).trim()} />
        </Suspense>
      );
    }
    const text = typeof children === 'string' ? children : String(children ?? '');
    const cleaned = text.replace(/`/g, '');
    const hasNewlines = cleaned.includes('\n');

    // Block-level code — rendered with background
    if (hasNewlines || className) {
      return (
        <code className={`${className ?? ''} block bg-[var(--bg-hover)] rounded-md px-3 py-2 my-2 text-[13px] whitespace-pre-wrap overflow-x-auto`}>
          {cleaned}
        </code>
      );
    }
    // Inline code
    return <code className="bg-[var(--bg-hover)] rounded px-1 py-0.5 text-[13px]">{cleaned}</code>;
  },
  pre({ children }: { children?: React.ReactNode }) {
    return <>{children}</>;
  },
};

/**
 * Persistent render cache for markdown bodies, keyed by the raw content string.
 *
 * react-markdown re-runs its whole remark/rehype pipeline on every mount, and
 * the message list remounts wholesale on every agent/thread switch (its React
 * key changes), so the `memo()` on MessageBubble — which only holds within one
 * mounted lifetime — never helps across a switch: each visible bubble re-parses
 * its markdown from scratch. Caching the rendered element tree by content
 * survives remounts, so revisiting a chat (or re-rendering identical content
 * after a background transcript refresh) reuses the already-parsed tree instead
 * of paying the parse again. This is the dominant reducible cost behind both the
 * "click into chat" and "switch between chats" lag.
 *
 * Soundness: the output is a pure function of the content string —
 * `hasLatexMath` and `escapeRawHtmlOutsideCode` are pure, and the plugin arrays
 * and `mdComponents` are module constants — so the string alone is a complete
 * key. React elements are immutable value descriptions, so reusing one across
 * mounts (or across two simultaneously-mounted bubbles) is safe; React
 * reconciles each location independently. Theming is CSS-variable driven, so a
 * cached tree restyles with the theme for free. Only settled messages flow
 * through here (streaming has its own renderer), so growing partials never
 * pollute the cache.
 */
const MD_RENDER_CACHE_LIMIT = 400;
const mdRenderCache = new Map<string, ReactElement>();

function renderMarkdown(content: string): ReactElement {
  const cached = mdRenderCache.get(content);
  if (cached !== undefined) {
    // Refresh recency (Map preserves insertion order → cheap LRU).
    mdRenderCache.delete(content);
    mdRenderCache.set(content, cached);
    return cached;
  }
  const useMath = hasLatexMath(content);
  const sanitized = escapeRawHtmlOutsideCode(content);
  const element = (
    <Markdown
      remarkPlugins={useMath ? remarkPluginsMath : remarkPluginsPlain}
      rehypePlugins={useMath ? rehypePluginsMath : rehypePluginsPlain}
      components={mdComponents}
    >
      {sanitized}
    </Markdown>
  );
  mdRenderCache.set(content, element);
  if (mdRenderCache.size > MD_RENDER_CACHE_LIMIT) {
    const oldest = mdRenderCache.keys().next().value;
    if (oldest !== undefined) mdRenderCache.delete(oldest);
  }
  return element;
}

/** Renders markdown, only enabling the expensive KaTeX math pipeline when
 *  the content actually contains LaTeX patterns (not currency $). Backed by a
 *  persistent render cache (see `renderMarkdown`) so remounts are cheap. */
function SafeMarkdown({ children }: { children: string }) {
  return renderMarkdown(children);
}

/** Render content with <mention> tags parsed into clickable bold elements.
 *  Text portions are rendered via SafeMarkdown for agent messages,
 *  or as plain text for user messages. */
function ContentWithMentions({ content, useMarkdown, onAccent }: { content: string; useMarkdown: boolean; onAccent?: boolean }) {
  const segments = useMemo(() => parseMentionSegments(content), [content]);
  const hasMentions = segments.some((s) => s.type === "mention");

  if (!hasMentions) {
    return useMarkdown ? <SafeMarkdown>{content}</SafeMarkdown> : <>{content}</>;
  }

  return (
    <>
      {segments.map((seg, i) =>
        seg.type === "mention" ? (
          <MentionTag key={i} id={seg.id} name={seg.name} onAccent={onAccent} />
        ) : useMarkdown ? (
          <SafeMarkdown key={i}>{seg.value}</SafeMarkdown>
        ) : (
          <span key={i}>{seg.value}</span>
        )
      )}
    </>
  );
}

// ---------------------------------------------------------------------------
// Image attachment thumbnails for user messages
// ---------------------------------------------------------------------------

export function ImageThumbnail({ attachment, agentId, attachmentFetcher }: { attachment: Attachment; agentId: string; attachmentFetcher?: AttachmentFetcher }) {
  const cached = useAttachmentStore((s) => s.attachmentCache.get(attachment.id));
  const loadAttachment = useAttachmentStore((s) => s.loadAttachment);
  const loadAttachmentWithFetcher = useAttachmentStore((s) => s.loadAttachmentWithFetcher);
  const [imgLoaded, setImgLoaded] = useState(false);
  const [imgError, setImgError] = useState(false);

  useEffect(() => {
    if (!cached || cached.status === "error") {
      if (attachmentFetcher) {
        loadAttachmentWithFetcher(agentId, attachment.id, attachmentFetcher);
      } else {
        loadAttachment(agentId, attachment.id);
      }
    }
  }, [agentId, attachment.id, cached, loadAttachment, loadAttachmentWithFetcher, attachmentFetcher]);

  const isLoading = !cached || cached.status === "loading" || (cached.status === "loaded" && !imgLoaded && !imgError);
  const imageUrl = attachmentFetcher ? attachmentFetcher.getUrl(attachment.id) : getAttachmentUrl(agentId, attachment.id);

  const handleClick = () => {
    if (!imgError) {
      useMediaPreviewStore.getState().openPreview({
        content: imageUrl,
        contentType: "image",
        filename: attachment.original_filename,
      });
    }
  };

  return (
    <button
      type="button"
      onClick={handleClick}
      className="relative w-[48px] h-[48px] rounded-lg overflow-hidden shadow-sm flex-shrink-0 cursor-pointer hover:opacity-80 transition-opacity"
      aria-label={`View ${attachment.original_filename}`}
    >
      {isLoading && !imgError && (
        <div className="absolute inset-0 flex items-center justify-center bg-[var(--bg-hover)]">
          <Loader2 size={16} className="animate-spin text-[var(--text-tertiary)]" />
        </div>
      )}
      {imgError ? (
        <div className="absolute inset-0 flex items-center justify-center bg-[var(--bg-hover)]">
          <ImageOff size={18} className="text-[var(--text-tertiary)]" />
        </div>
      ) : (
        <img
          src={imageUrl}
          alt={attachment.original_filename}
          className={`w-full h-full object-cover transition-opacity duration-200 ${imgLoaded ? "opacity-100" : "opacity-0"}`}
          onLoad={() => setImgLoaded(true)}
          onError={() => setImgError(true)}
        />
      )}
    </button>
  );
}

// ---------------------------------------------------------------------------
// File attachment pills for message bubbles
// ---------------------------------------------------------------------------

export function FileAttachmentTile({
  attachment,
  agentId,
  attachmentFetcher,
}: {
  attachment: Attachment;
  agentId: string;
  attachmentFetcher?: AttachmentFetcher;
}) {
  const cached = useAttachmentStore((s) => s.attachmentCache.get(attachment.id));
  const loadAttachment = useAttachmentStore((s) => s.loadAttachment);
  const loadAttachmentWithFetcher = useAttachmentStore((s) => s.loadAttachmentWithFetcher);
  const isMissing = cached?.status === "error";
  const isFolder = attachment.attachment_type === "folder";

  useEffect(() => {
    if (!cached) {
      if (attachmentFetcher) {
        loadAttachmentWithFetcher(agentId, attachment.id, attachmentFetcher);
      } else {
        loadAttachment(agentId, attachment.id);
      }
    }
  }, [agentId, attachment.id, cached, loadAttachment, loadAttachmentWithFetcher, attachmentFetcher]);

  const handleClick = () => {
    if (isMissing || isFolder) return;
    const a = document.createElement("a");
    a.href = attachmentFetcher ? attachmentFetcher.getUrl(attachment.id) : getAttachmentUrl(agentId, attachment.id);
    a.download = attachment.original_filename;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
  };

  const displayName = truncateFilename(attachment.original_filename);

  // Don't render missing/errored attachments — if the file is gone, don't show noise
  if (isMissing) return null;

  return (
    <button
      type="button"
      onClick={handleClick}
      disabled={isFolder}
      className={`flex-shrink-0 w-[56px] flex flex-col items-center ${
        isFolder ? "cursor-default" : "cursor-pointer hover:opacity-80"
      } transition-opacity`}
      title={isFolder ? attachment.file_path : attachment.original_filename}
    >
      <FileIcon
        fileName={attachment.original_filename}
        fileType={attachment.attachment_type}
      />
      <span className="text-[10px] text-[var(--text-primary)] opacity-70 text-center w-full truncate mt-[2px] px-[1px]">
        {displayName}
      </span>
    </button>
  );
}

function FileAttachmentRow({
  attachments,
  agentId,
  variant,
  attachmentFetcher,
}: {
  attachments: Attachment[];
  agentId: string;
  variant: "user" | "agent";
  attachmentFetcher?: AttachmentFetcher;
}) {
  return (
    <div className={`flex flex-wrap gap-1 ${variant === "user" ? "justify-end mb-1.5" : "mb-1.5"}`}>
      {attachments.map((att) => (
        <FileAttachmentTile key={att.id} attachment={att} agentId={agentId} attachmentFetcher={attachmentFetcher} />
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Artifact card — compact inline rendering of an artifact a message produced.
// Deliberately NOT a live iframe for every card: a scrollback thread full of
// historical artifacts would otherwise mount a sandboxed renderer per card,
// which is a real performance trap — so
// a card reloaded from history stays inert (title + kind badge) until the
// user opens it. A card the agent just published THIS turn is the one thing
// the user is actively waiting to see, so it defaults open instead (see
// `useArtifactStore`'s `liveIds`). Either way it mounts
// `ArtifactPreview` — the same renderer the Assets panel and the pop-out
// window use — scoped to just this one expanded card.
// ---------------------------------------------------------------------------

function artifactCardKindLabel(kind: ArtifactKind): string {
  return kind === "html" ? "HTML" : kind.charAt(0).toUpperCase() + kind.slice(1);
}

/** Renders one inline artifact card by id. Resolves its title/kind/
 *  refresh_intent from `useArtifactStore`'s `cardsById` registry — populated
 *  directly from the `ArtifactWrite` tool result (live or persisted), so the
 *  collapsed header never waits on a `listArtifacts` fetch. Falls back to the
 *  per-agent artifact list (already loaded for the Assets panel) for the rare
 *  case a card stub wasn't registered — e.g. an artifact from before this
 *  feature shipped. */
export function ArtifactCardTile({ artifactId, agentId }: { artifactId: string; agentId: string }) {
  // Lazy initializer so this only reads the live-set once, at mount — not a
  // reactive subscription, so a manual collapse afterward always wins.
  const [expanded, setExpanded] = useState(() => useArtifactStore.getState().liveIds.has(artifactId));
  // Bumped by the refresh action to force `ArtifactPreview` to remount (and
  // therefore re-fetch) rather than reuse whatever it last rendered.
  const [refreshKey, setRefreshKey] = useState(0);

  const stub = useArtifactStore((s) => s.getCard(artifactId));
  const fallback = useArtifactStore((s) => s.byAgent.get(agentId)?.artifacts.find((a) => a.id === artifactId));

  useEffect(() => {
    if (stub || fallback) return;
    useArtifactStore.getState().loadArtifacts(agentId);
  }, [stub, fallback, agentId]);

  const title = stub?.title ?? fallback?.title ?? "Artifact";
  const kind: ArtifactKind = stub?.kind ?? fallback?.kind ?? "unknown";
  const refreshIntent: RefreshIntent = stub?.refresh_intent ?? fallback?.refresh_intent ?? "none";
  const Icon = kind === "html" ? Code2 : Box;

  const toggleExpanded = () => setExpanded((v) => !v);

  const handlePopOut = (e: React.MouseEvent) => {
    e.stopPropagation();
    openArtifactWindow(agentId, artifactId);
  };

  const handleRefresh = (e: React.MouseEvent) => {
    e.stopPropagation();
    setExpanded(true);
    setRefreshKey((k) => k + 1);
  };

  return (
    // No `max-w-[...]` here — the enclosing agent bubble (`MessageBubbleProps`
    // render below) already caps prose width at 85-95%; stacking a second,
    // identical percentage cap on top of that compounds down to ~72-90% of an
    // already-narrow column, which is what made structured content (tables
    // especially) look cramped with dead space on both sides instead of
    // using the width the bubble already allows it. `w-full` here just means
    // "fill what the bubble gives you."
    <div className="my-[6px] w-full" data-testid="artifact-card-tile">
      <div
        role="button"
        tabIndex={0}
        onClick={toggleExpanded}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            toggleExpanded();
          }
        }}
        aria-label={expanded ? `Collapse ${title}` : `Expand ${title}`}
        aria-expanded={expanded}
        className="flex items-center gap-[8px] px-[10px] py-[8px] rounded-[10px] border cursor-pointer hover:bg-[var(--bg-hover)] transition-colors"
        style={{ borderColor: "var(--border-secondary)", backgroundColor: "var(--bg-secondary)" }}
      >
        <div className="w-[26px] h-[26px] rounded-[7px] bg-[var(--bg-hover)] flex items-center justify-center flex-shrink-0">
          <Icon size={13} style={{ color: "var(--text-secondary)" }} />
        </div>
        <div className="flex-1 min-w-0">
          <div className="text-[12px] font-semibold truncate" style={{ color: "var(--text-primary)" }}>
            {title}
          </div>
          <div className="text-[10px] uppercase tracking-wide" style={{ color: "var(--text-tertiary)" }}>
            {artifactCardKindLabel(kind)}
          </div>
        </div>
        <div className="flex items-center gap-[2px] flex-shrink-0">
          {refreshIntent !== "none" && (
            <button
              type="button"
              onClick={handleRefresh}
              aria-label="Refresh artifact"
              className="w-[22px] h-[22px] rounded-[6px] flex items-center justify-center hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
            >
              <RotateCw size={12} style={{ color: "var(--text-secondary)" }} />
            </button>
          )}
          <button
            type="button"
            onClick={handlePopOut}
            aria-label="Open artifact in new window"
            className="w-[22px] h-[22px] rounded-[6px] flex items-center justify-center hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
          >
            <ExternalLink size={12} style={{ color: "var(--text-secondary)" }} />
          </button>
          <ChevronDown
            size={14}
            style={{
              color: "var(--text-secondary)",
              transform: expanded ? "rotate(180deg)" : undefined,
              transition: "transform 0.15s ease",
            }}
          />
        </div>
      </div>

      {expanded && (
        <div
          className="relative mt-[6px] rounded-[12px] overflow-hidden border"
          style={{ borderColor: "var(--border-secondary)", height: 340 }}
          data-testid="artifact-card-expanded"
        >
          <ArtifactPreview
            key={refreshKey}
            agentId={agentId}
            artifactId={artifactId}
            onClose={() => setExpanded(false)}
            onPopOut={() => openArtifactWindow(agentId, artifactId)}
          />
        </div>
      )}
    </div>
  );
}

/** A single visual piece inside a coalesced agent bubble. Either rendered agent
 *  text or an inline skill-load action chip. */
export type CoalescedSegment =
  | { kind: "text"; content: string }
  | { kind: "chip"; skillName: string; success: boolean };

/** Inline chip rendered between text segments in a coalesced bubble. */
export function SkillLoadChip({ skillName, success }: { skillName: string; success: boolean }) {
  return (
    <span
      className={`inline-flex items-center gap-[6px] my-[6px] px-[10px] py-[3px] rounded-full text-[12px] border ${
        success
          ? "bg-[var(--bg-hover)] text-[var(--text-secondary)] border-[var(--border-secondary)]"
          : "bg-[var(--bg-hover)] text-red-500 border-red-500/40"
      }`}
      title={success ? `Loaded skill: ${skillName}` : `Failed to load skill: ${skillName}`}
    >
      <Sparkles size={12} className="opacity-80" />
      <span>{success ? `Loaded skill: ${skillName}` : `Failed to load skill: ${skillName}`}</span>
    </span>
  );
}

interface MessageBubbleProps {
  entry: TranscriptEntry;
  agentName: string;
  agentEmoji: string;
  agentId?: string;
  /** Optional custom fetcher for non-agent attachment contexts (e.g. team attachments). */
  attachmentFetcher?: AttachmentFetcher;
  /** When true, this message is rendered as a continuation of the previous one
   *  from the same sender — agent messages hide the avatar + name label. */
  groupWithPrevious?: boolean;
  /** When provided, the bubble renders these segments in order instead of a
   *  single `entry.content` block. Used by MessageList to merge an assistant
   *  bubble with any following skill_load chips (and the agent's follow-up
   *  response) into one continuous bubble. */
  coalescedSegments?: CoalescedSegment[];
  /** Enables the "Branch from here" affordance. Only the live agent chat sets
   *  this — pinned overlays, team attachments, and project channels leave it
   *  off because branching has no meaning there. */
  allowBranch?: boolean;
  /** Ids of artifacts this turn produced, resolved by `MessageList` (live-
   *  finalized `metadata.artifact_ids` and/or the persisted `ArtifactWrite`
   *  tool_result scan — see `extractArtifactWriteResults`). Renders one
   *  `ArtifactCardTile` per id, above the reply text. */
  artifactIds?: string[];
}

/** Format a timestamp string to HH:MM. */
function formatTime(ts: string): string {
  const date = new Date(ts);
  if (isNaN(date.getTime())) return "";
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
}

function useMessageStatus(entry: TranscriptEntry): "sending" | "sent" | "delivered" | "seen" | "error" | null {
  // Single derived selector — only re-renders when the computed status actually changes
  return useChatStore((s) => {
    if (!isUserMessage(entry)) return null;

    const messageId = s.messageIdByTs[entry.ts] || (entry.metadata?.message_id as string | undefined);

    if (messageId) {
      if (s.seenMessageIds.has(messageId)) return "seen";
      if (s.pendingMessageIds.has(messageId)) return "sent";
    }

    if (entry.status) return entry.status;
    if (messageId) return "delivered";

    return "sending";
  });
}

/** Character thresholds for "Read more" truncation */
const USER_MSG_TRUNCATE_CHARS = 300;
const AGENT_MSG_TRUNCATE_CHARS = 800;
/** Max-height in px when collapsed (approximate visual lines) */
const USER_MSG_MAX_HEIGHT = 130;   // ~6 lines at 1.45 line-height
const AGENT_MSG_MAX_HEIGHT = 340;  // ~15 lines

/** Check if a timestamp is from the current calendar day (local time). */
function isToday(ts: string): boolean {
  const d = new Date(ts);
  const now = new Date();
  return d.getFullYear() === now.getFullYear() && d.getMonth() === now.getMonth() && d.getDate() === now.getDate();
}

export const MessageBubble = memo(function MessageBubble({ entry, agentName, agentEmoji, agentId, attachmentFetcher, groupWithPrevious = false, coalescedSegments, allowBranch = false, artifactIds }: MessageBubbleProps) {
  const userMsg = isUserMessage(entry);
  const time = formatTime(entry.ts);
  const status = useMessageStatus(entry);
  const isDark = useIsDark();
  const [branching, setBranching] = useState(false);
  const bubbleColor = useUserPreferencesStore((s) => s.bubbleColor);
  const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);
  // Hidden agents (co-pilots, inline coordinators) carry an `owning_team_id`.
  // On those surfaces, the avatar should not be a doorway into the agent
  // profile modal — the underlying agent is meant to be opaque to the user.
  const isHiddenAgentSurface = useChatStore((s) => !!s.selectedAgentProfile?.owning_team_id);
  const bookmarked = useBookmarkStore((s) => s.bookmarkedTimestamps.has(entry.ts));
  const toggleBookmark = useBookmarkStore((s) => s.toggleBookmark);
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);

  const handleCopy = () => {
    if (!entry.content) return;
    navigator.clipboard.writeText(entry.content);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  // Determine if this message should be truncatable
  const shouldTruncate = useMemo(() => {
    if (expanded) return false;
    // Coalesced bubbles render a mix of agent text + inline chips; skip
    // truncation so the chip placement stays readable.
    if (coalescedSegments && coalescedSegments.length > 0) return false;
    const len = entry.content?.length ?? 0;
    if (userMsg) return len > USER_MSG_TRUNCATE_CHARS;
    // Agent messages: only truncate if NOT from today
    return len > AGENT_MSG_TRUNCATE_CHARS && !isToday(entry.ts);
  }, [entry.content, entry.ts, userMsg, expanded, coalescedSegments]);

  // Extract attachments from metadata
  const allAttachments = useMemo(() => {
    if (!entry.metadata) return [];
    return ((entry.metadata as Record<string, unknown>)?.attachments as Attachment[] | undefined) ?? [];
  }, [entry.metadata]);

  // Suppress bubbles whose visible text is whitespace-only after stripping
  // agent-action tags and whose attachment list is empty. This hides the
  // pre-recall empty bubble when recall_history is emitted with no preamble.
  const hasVisibleContent = useMemo(() => {
    if (coalescedSegments && coalescedSegments.length > 0) return true;
    if (allAttachments.length > 0) return true;
    if (artifactIds && artifactIds.length > 0) return true;
    return stripAgentActionTags(entry.content ?? "").trim().length > 0;
  }, [entry.content, allAttachments, artifactIds, coalescedSegments]);

  const imageAttachments = useMemo(() => {
    if (!userMsg) return [];
    return allAttachments.filter((a) => a.attachment_type === "image");
  }, [userMsg, allAttachments]);

  const fileAttachments = useMemo(() => {
    return allAttachments.filter((a) => a.attachment_type !== "image");
  }, [allAttachments]);

  // Extract agent ID from the role object for navigation
  const agentRoleId = !userMsg && typeof entry.role === "object" && "agent" in entry.role
    ? (entry.role as { agent: string }).agent
    : null;

  const handleBookmarkClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    if (agentId) toggleBookmark(agentId, entry);
  };

  const handleBranchClick = async (e: React.MouseEvent) => {
    e.stopPropagation();
    if (!agentId || branching) return;
    setBranching(true);
    try {
      await branchFromMessage(agentId, entry);
    } catch (err) {
      console.error("[MessageBubble] branchFromMessage failed:", err);
    } finally {
      setBranching(false);
    }
  };

  if (!hasVisibleContent) return null;

  // "Branch from here" — forks a new thread that inherits history up to this
  // message, then shows the branch's transcript. Only rendered for the live
  // agent chat (allowBranch) where a concrete agent + thread context exists.
  const branchButton = allowBranch && agentId ? (
    <button
      type="button"
      onClick={handleBranchClick}
      disabled={branching}
      className="inline-flex items-center p-1 rounded cursor-pointer hover:bg-[var(--bg-hover)] transition-opacity opacity-0 group-hover/msg:opacity-100 disabled:opacity-40"
      aria-label="Branch from here"
      title="Branch a new thread from this message"
    >
      <GitBranch size={16} className="text-[var(--text-secondary)]" />
    </button>
  ) : null;

  const bookmarkButton = (_variant: "user" | "agent") => agentId ? (
    <button
      type="button"
      onClick={handleBookmarkClick}
      className={`inline-flex items-center p-1 rounded cursor-pointer hover:bg-[var(--bg-hover)] transition-opacity ${bookmarked ? "opacity-100" : "opacity-0 group-hover/msg:opacity-100"}`}
      aria-label={bookmarked ? "Remove bookmark" : "Add bookmark"}
    >
      <Bookmark
        size={16}
        className="text-[var(--text-secondary)]"
        fill={bookmarked ? "currentColor" : "none"}
      />
    </button>
  ) : null;

  if (userMsg) {
    const entityId = agentId || (attachmentFetcher ? "custom" : undefined);
    const hasAttachments = (imageAttachments.length > 0 || fileAttachments.length > 0) && entityId;
    return (
      <div className="group/msg flex flex-col items-end">
        {hasAttachments && (
          <div className="flex flex-wrap gap-1 justify-end mb-1.5">
            {imageAttachments.map((att) => (
              <ImageThumbnail key={att.id} attachment={att} agentId={entityId!} attachmentFetcher={attachmentFetcher} />
            ))}
            {fileAttachments.map((att) => (
              <FileAttachmentTile key={att.id} attachment={att} agentId={entityId!} attachmentFetcher={attachmentFetcher} />
            ))}
          </div>
        )}
        <div
          className="max-w-[85%] @3xl:max-w-[75%] @5xl:max-w-[70%] px-[14px] py-[10px] text-[var(--text-user-message)] text-[15px] leading-[1.45] whitespace-pre-wrap break-words"
          style={{ borderRadius: "16px 4px 16px 16px", backgroundColor: adjustBubbleColor(bubbleColor || "#1164A3", isDark) }}
        >
          <div className="relative">
            <div
              style={shouldTruncate ? { maxHeight: `${USER_MSG_MAX_HEIGHT}px`, overflow: "hidden" } : undefined}
            >
              <ContentWithMentions content={entry.content} useMarkdown={false} onAccent />
            </div>
            {shouldTruncate && (
              <div
                className="absolute bottom-0 left-0 right-0 h-[40px] pointer-events-none"
                style={{ background: `linear-gradient(transparent, ${adjustBubbleColor(bubbleColor || "#1164A3", isDark)})` }}
              />
            )}
          </div>
          {shouldTruncate && (
            <button
              type="button"
              onClick={() => setExpanded(true)}
              className="mt-[4px] text-[13px] font-bold text-white/80 hover:text-white cursor-pointer"
            >
              Read more
            </button>
          )}
          {expanded && (entry.content?.length ?? 0) > USER_MSG_TRUNCATE_CHARS && (
            <button
              type="button"
              onClick={() => setExpanded(false)}
              className="mt-[4px] text-[13px] font-bold text-white/80 hover:text-white cursor-pointer"
            >
              Show less
            </button>
          )}
        </div>
        <div className="flex items-center gap-[8px] mt-[4px] mr-[4px]">
          <button
            type="button"
            onClick={handleCopy}
            className="inline-flex items-center p-1 rounded cursor-pointer hover:bg-[var(--bg-hover)] transition-opacity opacity-0 group-hover/msg:opacity-100"
            aria-label="Copy message"
          >
            {copied ? (
              <Check size={16} className="text-[var(--text-secondary)]" />
            ) : (
              <Copy size={16} className="text-[var(--text-secondary)]" />
            )}
          </button>
          {bookmarkButton("user")}
          {branchButton}
          {time && (
            <span className="text-[11px] text-[var(--text-tertiary)]">{time}</span>
          )}
          {status && <StatusIndicator status={status} />}
        </div>
      </div>
    );
  }

  // Agent message — clickable emoji avatar on the left, bubble on the right.
  // Continuation messages from the same sender hide the avatar + name and
  // reserve the avatar slot as whitespace so bubbles stay aligned.
  return (
    <div className="group/msg flex items-start gap-[10px]">
      {groupWithPrevious ? (
        <div className="flex-shrink-0 w-[34px]" aria-hidden="true" />
      ) : (
        <button
          type="button"
          onClick={() => !isHiddenAgentSurface && agentRoleId && useAgentProfileModalStore.getState().openEdit(agentRoleId)}
          disabled={!agentRoleId || isHiddenAgentSurface}
          title={isHiddenAgentSurface ? agentName : `Edit ${agentName}`}
          className={`flex-shrink-0 w-[34px] h-[34px] ${circularAvatars ? 'rounded-full' : 'rounded-[10px]'} flex items-center justify-center text-[18px] select-none mt-[2px] border-2 border-transparent hover:border-[var(--accent)] hover:scale-105 active:scale-95 transition-all cursor-pointer disabled:cursor-default disabled:hover:border-transparent disabled:hover:scale-100`}
          style={{ backgroundColor: agentAvatarColor(agentName, isDark) }}
          aria-label={isHiddenAgentSurface ? agentName : `Edit ${agentName}`}
        >
          {agentEmoji}
        </button>
      )}

      {/* Bubble + name + timestamp */}
      <div className="flex flex-col items-start min-w-0">
        {!groupWithPrevious && (
          <span className="text-[14px] font-bold text-[var(--text-primary)] mb-[4px]">{agentName}</span>
        )}
        <div
          className={`${
            // A bubble carrying an artifact is no longer just prose — cap it
            // at the reading-friendly 85-95% width only when there's no
            // artifact card to accommodate. With one, let it fill the column
            // so structured content (tables especially) gets real width
            // instead of being squeezed into a paragraph-width box.
            artifactIds && artifactIds.length > 0 ? "max-w-full" : "max-w-[95%] @3xl:max-w-[90%] @5xl:max-w-[85%]"
          } px-[14px] py-[10px] bg-[var(--bg-agent-message)] text-[15px] text-[var(--text-primary)] leading-[1.45] break-words prose prose-sm prose-neutral`}
          style={{ borderRadius: "4px 16px 16px 16px" }}
        >
          {fileAttachments.length > 0 && (agentId || attachmentFetcher) && (
            <FileAttachmentRow attachments={fileAttachments} agentId={agentId || "custom"} variant="agent" attachmentFetcher={attachmentFetcher} />
          )}
          {/* Artifact cards sit at the top of the bubble — the produced card(s)
              render on top of the agent's reply text, not buried below it.
              Keyed by id so the handoff from the streaming bubble (live
              `artifactIds`) to the finalized one (same ids, now off
              `metadata.artifact_ids`) never double-mounts a card. */}
          {artifactIds && agentId &&
            artifactIds.map((id) => <ArtifactCardTile key={id} artifactId={id} agentId={agentId} />)}
          <div className="relative">
            <div
              style={shouldTruncate ? { maxHeight: `${AGENT_MSG_MAX_HEIGHT}px`, overflow: "hidden" } : undefined}
            >
              {coalescedSegments && coalescedSegments.length > 0 ? (
                coalescedSegments.map((seg, i) =>
                  seg.kind === "text" ? (
                    <ContentWithMentions key={`seg-${i}`} content={seg.content} useMarkdown={true} />
                  ) : (
                    <div key={`seg-${i}`}>
                      <SkillLoadChip skillName={seg.skillName} success={seg.success} />
                    </div>
                  )
                )
              ) : (
                <ContentWithMentions content={entry.content} useMarkdown={true} />
              )}
            </div>
            {shouldTruncate && (
              <div
                className="absolute bottom-0 left-0 right-0 h-[40px] pointer-events-none"
                style={{ background: "linear-gradient(transparent, var(--bg-agent-message))" }}
              />
            )}
          </div>
          {shouldTruncate && (
            <button
              type="button"
              onClick={() => setExpanded(true)}
              className="mt-[6px] text-[13px] font-bold text-[var(--accent)] hover:underline cursor-pointer"
            >
              Read more
            </button>
          )}
          {expanded && (entry.content?.length ?? 0) > AGENT_MSG_TRUNCATE_CHARS && !isToday(entry.ts) && (
            <button
              type="button"
              onClick={() => setExpanded(false)}
              className="mt-[6px] text-[13px] font-bold text-[var(--accent)] hover:underline cursor-pointer"
            >
              Show less
            </button>
          )}
        </div>
        <div className="flex items-center gap-[8px] mt-[4px]">
          {time && (
            <span className="text-[11px] text-[var(--text-secondary)]">{time}</span>
          )}
          <button
            type="button"
            onClick={handleCopy}
            className="inline-flex items-center p-1 rounded cursor-pointer hover:bg-[var(--bg-hover)] transition-opacity opacity-0 group-hover/msg:opacity-100"
            aria-label="Copy message"
          >
            {copied ? (
              <Check size={16} className="text-[var(--text-secondary)]" />
            ) : (
              <Copy size={16} className="text-[var(--text-secondary)]" />
            )}
          </button>
          {bookmarkButton("agent")}
          {branchButton}
        </div>
      </div>
    </div>
  );
});
