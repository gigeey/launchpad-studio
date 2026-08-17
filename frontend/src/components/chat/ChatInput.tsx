import { useState, useRef, useEffect, useCallback, type ReactNode } from "react";
import { createPortal } from "react-dom";

import { twMerge } from "tailwind-merge";
import { motion, AnimatePresence } from "framer-motion";
import { useEditor, EditorContent } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import Placeholder from "@tiptap/extension-placeholder";
import {
  Paperclip,
  Loader2,
  X,
  AlertCircle,
  FolderOpen,
  File,
  ArrowUp,
  Focus,
  ChevronRight,
  ImageOff,
} from "lucide-react";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import { useFocusPathStore } from "../../stores/focusPathStore";
import { readFile, exists, stat } from "@tauri-apps/plugin-fs";
import { MentionExtension } from "./MentionExtension";
import { MentionAutocomplete } from "./MentionAutocomplete";
import { SlashCommandPopover } from "./SlashCommandPopover";
import { FileIcon } from "./FileIcon";
import { TeamMember, PendingAttachment, AttachmentType, Attachment } from "../../types/api";
import { DraftAttachment } from "../../stores/draftStore";
import { useMediaPreviewStore } from "../../stores/mediaPreviewStore";
import { useWorkflowStore } from "../../stores/workflowStore";
import { useAgentCommandStore } from "../../stores/agentCommandStore";
import { useSkillStore } from "../../stores/skillStore";
import { useChatStore } from "../../stores/chatStore";

import * as api from "../../lib/api";
import { useTaskCreateModalStore } from "../../stores/taskCreateModalStore";

const EMPTY_COMMANDS: api.AgentCommand[] = [];
const EMPTY_SKILLS: api.Skill[] = [];

/* ── File tooltip (matches menu tooltip style) ── */
let _ftWarm = false;
let _ftWarmTimer: ReturnType<typeof setTimeout> | null = null;

function FileTooltip({ children, label }: { children: ReactNode; label: string }) {
  const [show, setShow] = useState(false);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const anchorRef = useRef<HTMLDivElement>(null);

  const updatePos = () => {
    if (!anchorRef.current) return;
    const rect = anchorRef.current.getBoundingClientRect();
    setPos({ top: rect.top - 8, left: rect.left + rect.width / 2 });
  };

  const enter = () => {
    updatePos();
    if (_ftWarm) {
      setShow(true);
    } else {
      timer.current = setTimeout(() => { updatePos(); setShow(true); _ftWarm = true; }, 700);
    }
    if (_ftWarmTimer) clearTimeout(_ftWarmTimer);
  };

  const leave = () => {
    if (timer.current) clearTimeout(timer.current);
    setShow(false);
    _ftWarmTimer = setTimeout(() => { _ftWarm = false; }, 500);
  };

  useEffect(() => () => { if (timer.current) clearTimeout(timer.current); }, []);

  return (
    <div ref={anchorRef} onMouseEnter={enter} onMouseLeave={leave}>
      {children}
      {show && pos && createPortal(
        <AnimatePresence>
          <div style={{ position: "fixed", top: pos.top, left: pos.left, transform: "translate(-50%, -100%)", pointerEvents: "none", zIndex: 9999 }}>
            <motion.div
              initial={{ opacity: 0, scale: 0.95, y: 4 }}
              animate={{ opacity: 1, scale: 1, y: 0 }}
              exit={{ opacity: 0, scale: 0.95, y: 4 }}
              transition={{ duration: 0.15, ease: "easeOut" }}
              className="px-2 py-1 text-xs font-medium text-[var(--bg-primary)] bg-[var(--text-primary)] rounded shadow-lg whitespace-nowrap pointer-events-none"
            >
              {label}
              <div className="absolute top-full left-1/2 -translate-x-1/2 -mt-1 w-2 h-2 bg-[var(--text-primary)] rotate-45" />
            </motion.div>
          </div>
        </AnimatePresence>,
        document.body
      )}
    </div>
  );
}

/** Image thumbnail with onError fallback for draft attachments.
 *  Mirrors the AssetsPanel pattern (ImageOff on load failure) so UI is
 *  consistent and we never leak a broken-image icon. */
function DraftImageThumbnail({
  src,
  alt,
  onClick,
}: {
  src: string;
  alt: string;
  onClick: () => void;
}) {
  const [imgError, setImgError] = useState(false);
  if (imgError) {
    return (
      <div className="w-full h-full flex items-center justify-center">
        <ImageOff size={20} className="text-[var(--text-tertiary)]" />
      </div>
    );
  }
  return (
    <img
      src={src}
      alt={alt}
      className="w-full h-full object-cover"
      onClick={onClick}
      onError={() => setImgError(true)}
    />
  );
}

/** Determine the effective attachment type from a PendingAttachment */
function getEffectiveType(pa: PendingAttachment): AttachmentType | undefined {
  if (pa.attachment) return pa.attachment.attachment_type;
  if (pa.isFolder) return "folder";
  if (pa.file?.type.startsWith("image/")) return "image";
  // Fallback: detect image by file extension when mime type is missing/generic
  const fileName = pa.file?.name?.toLowerCase() ?? "";
  if (/\.(png|jpe?g|gif|webp|svg|bmp|ico|avif)$/.test(fileName)) return "image";
  if (pa.file?.type.includes("spreadsheet") || pa.file?.type.includes("excel") || /\.(xlsx?|csv|tsv)$/.test(fileName)) return "spreadsheet";
  if (pa.file?.type.includes("pdf") || pa.file?.type.includes("word") || pa.file?.type.includes("document") || /\.(pdf|docx?|rtf)$/.test(fileName)) return "document";
  if (pa.file?.type.startsWith("text/") || /\.(ts|tsx|js|jsx|py|rs|go|rb|java|c|cpp|h|hpp|sh|yaml|yml|toml|json|xml|html|css|scss|sql|md)$/.test(fileName)) return "code";
  return "other";
}

const ACCEPTED_MIME_TYPES = [
  "image/png",
  "image/jpeg",
  "image/gif",
  "image/webp",
  "application/pdf",
  "text/*",
  "application/msword",
  "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
  "application/vnd.ms-excel",
  "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  "application/vnd.ms-powerpoint",
  "application/vnd.openxmlformats-officedocument.presentationml.presentation",
].join(",");

/** MIME type lookup by file extension */
const EXT_TO_MIME: Record<string, string> = {
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif",
  webp: "image/webp", bmp: "image/bmp", svg: "image/svg+xml",
  pdf: "application/pdf",
  doc: "application/msword",
  docx: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
  xls: "application/vnd.ms-excel",
  xlsx: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  ppt: "application/vnd.ms-powerpoint",
  pptx: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
  txt: "text/plain", md: "text/markdown", csv: "text/csv",
  json: "application/json", xml: "application/xml",
  yaml: "text/yaml", yml: "text/yaml", toml: "text/plain",
  html: "text/html", css: "text/css",
  js: "text/javascript", jsx: "text/javascript",
  ts: "text/typescript", tsx: "text/typescript",
  py: "text/x-python", rb: "text/x-ruby", rs: "text/x-rust",
  go: "text/x-go", java: "text/x-java",
  c: "text/x-c", cpp: "text/x-c++", h: "text/x-c", hpp: "text/x-c++",
  sh: "text/x-shellscript", bash: "text/x-shellscript", zsh: "text/x-shellscript",
  log: "text/plain",
};

/**
 * Read a local file using Tauri FS and return a proper File object for upload.
 * Resolves ~ to home dir.
 */
async function readLocalFileAsBlob(filePath: string): Promise<File | null> {
  try {
    // Resolve ~ to home dir
    let resolvedPath = filePath;
    if (resolvedPath.startsWith("~")) {
      // Tauri FS doesn't auto-resolve ~, use $HOME
      const home = await import("@tauri-apps/api/path").then((m) => m.homeDir());
      resolvedPath = resolvedPath.replace(/^~/, home);
    }

    const fileExists = await exists(resolvedPath);
    if (!fileExists) return null;

    const fileInfo = await stat(resolvedPath);
    if (fileInfo.isDirectory) return null; // directories handled separately

    const bytes = await readFile(resolvedPath);
    const filename = resolvedPath.split("/").pop() || "file";
    const ext = filename.split(".").pop()?.toLowerCase() || "";
    const mimeType = EXT_TO_MIME[ext] || "application/octet-stream";

    return new window.File([bytes], filename, { type: mimeType });
  } catch {
    return null;
  }
}

/** Module-level registry for in-flight upload File data. Keyed by pending ID.
 *  Persists across conversation switches in the same session but not across
 *  page reloads (by design — Files cannot be serialized to localStorage). */
interface InFlightUpload {
  file: File;
  isFolder: boolean;
  folderPath?: string;
}
const uploadingFilesRegistry = new Map<string, InFlightUpload>();

/** Convert saved DraftAttachment[] back to PendingAttachment[] for restoring state. */
function draftAttachmentsToPending(
  drafts: DraftAttachment[] | undefined,
  agentId: string | undefined,
): PendingAttachment[] {
  if (!drafts || drafts.length === 0) return [];
  const result: PendingAttachment[] = [];
  for (const da of drafts) {
    if (da.status === "uploading") {
      const inflight = uploadingFilesRegistry.get(da.pendingId);
      if (!inflight) {
        // File data was not preserved across reload — drop the entry.
        continue;
      }
      const isImage = inflight.file.type.startsWith("image/")
        || /\.(png|jpe?g|gif|webp|svg|bmp)$/i.test(inflight.file.name);
      result.push({
        id: da.pendingId,
        file: inflight.file,
        previewUrl: isImage ? URL.createObjectURL(inflight.file) : null,
        status: "uploading",
        serverId: null,
        attachment: null,
        isFolder: inflight.isFolder,
        folderPath: inflight.folderPath,
      });
      continue;
    }
    const isImage = da.attachment.attachment_type === "image";
    result.push({
      id: `restored-${da.serverId}`,
      file: new window.File([], da.attachment.original_filename),
      previewUrl: isImage && agentId
        ? api.getAttachmentUrl(agentId, da.serverId)
        : null,
      status: "uploaded" as const,
      serverId: da.serverId,
      attachment: da.attachment,
      isFolder: da.isFolder,
      folderPath: da.folderPath,
    });
  }
  return result;
}

/** Convert current PendingAttachment[] to serializable DraftAttachment[]. Also
 *  registers the File of any uploading attachment in an in-memory map so that
 *  it can be restored and re-uploaded on the next mount of ChatInput. */
function pendingToDraftAttachments(pending: PendingAttachment[]): DraftAttachment[] {
  const result: DraftAttachment[] = [];
  for (const p of pending) {
    if (p.status === "uploaded" && p.serverId && p.attachment) {
      result.push({
        status: "uploaded",
        serverId: p.serverId,
        attachment: p.attachment,
        isFolder: p.isFolder,
        folderPath: p.folderPath,
      });
      continue;
    }
    if (p.status === "uploading" && p.file && p.file.size > 0) {
      uploadingFilesRegistry.set(p.id, {
        file: p.file,
        isFolder: p.isFolder,
        folderPath: p.folderPath,
      });
      result.push({
        status: "uploading",
        pendingId: p.id,
        filename: p.file.name,
        mimeType: p.file.type || "application/octet-stream",
        isFolder: p.isFolder,
        folderPath: p.folderPath,
      });
    }
    // "pending" and "error" statuses are dropped.
  }
  return result;
}

interface ChatInputProps {
  onSend: (content: string, attachmentIds?: string[], attachments?: Attachment[]) => void;
  placeholder?: string;
  disabled?: boolean;
  initialDraft?: string;
  /** HTML draft content preserving rich elements like @mention pills */
  initialDraftHtml?: string;
  /** Restored draft attachments (already uploaded on the server). */
  initialDraftAttachments?: DraftAttachment[];
  onUnmount?: (text: string, html: string, conversationId: string) => void;
  /** Called on unmount / conversation change with the current uploaded attachments. */
  onUnmountAttachments?: (attachments: DraftAttachment[], conversationId: string) => void;
  /** Stable ID for the current conversation — changing this resets the input without remounting. */
  conversationId?: string;
  /** When provided, typing @ triggers an autocomplete dropdown for team members. */
  teamMembers?: TeamMember[];
  /** Map of agent_id → display name for the mention picker. */
  agentNameMap?: Record<string, string>;
  /** When true, shows a stop button on the right side of the input. */
  isProcessing?: boolean;
  /** Called when the stop button is clicked. */
  onStop?: () => void;
  /** The agent ID for the current conversation (needed for attachment uploads). */
  agentId?: string;
  /** Whether the agent supports file attachments. */
  fileCapabilitiesSupported?: boolean;
  /** Generic entity ID used as guard for attachment operations. Defaults to agentId. */
  entityId?: string;
  /** Key used for the focus-path store. Defaults to agentId. Use a composite key
   *  (e.g. `team:${teamId}`) to scope focus state per-team instead of per-agent. */
  focusStoreKey?: string;
  /** Explicit working directory override. When provided, used instead of
   *  selectedAgentProfile.working_dir (which may be stale in team context). */
  defaultWorkingDir?: string | null;
  /** Custom upload handler — when provided, used instead of api.uploadAttachment(agentId, file). */
  onUploadAttachment?: (file: File) => Promise<Attachment>;
  /** Custom folder reference handler — when provided, used instead of api.addFolderReference(agentId, path). */
  onAddFolderReference?: (path: string) => Promise<Attachment>;
  /** Custom delete handler — when provided, used instead of api.deleteAttachment(agentId, attachmentId). */
  onDeleteAttachment?: (attachmentId: string) => Promise<void>;
}

export function ChatInput({
  onSend,
  placeholder = "Send a message...",
  disabled = false,
  initialDraft = "",
  initialDraftHtml,
  initialDraftAttachments,
  onUnmount,
  onUnmountAttachments,
  conversationId,
  teamMembers,
  agentNameMap,
  isProcessing = false,
  onStop,
  agentId,
  fileCapabilitiesSupported = false,
  entityId: entityIdProp,
  focusStoreKey: focusStoreKeyProp,
  defaultWorkingDir: defaultWorkingDirProp,
  onUploadAttachment,
  onAddFolderReference,
  onDeleteAttachment,
}: ChatInputProps) {

  const [isMultiLine, setIsMultiLine] = useState(false);
  const [borderFlash, setBorderFlash] = useState(false);
  const [hasTextContent, setHasTextContent] = useState(!!initialDraft);
  const [charCount, setCharCount] = useState(0);
  // Mirrors the live length without forcing a re-render — the counter is only
  // painted near the limit, so most keystrokes shouldn't re-render this tree.
  const charCountRef = useRef(0);
  const MAX_CHARS = 25000;
  // The counter (and its red over-limit styling) only appears past 80% of the
  // cap, so that's the only band where the length actually needs to be state.
  const CHAR_COUNT_VISIBLE_AT = MAX_CHARS * 0.8;
  const [isDragOver, setIsDragOver] = useState(false);
  const [isCancelling, setIsCancelling] = useState(false);
  const dragCounterRef = useRef(0);
  const containerRef = useRef<HTMLDivElement>(null);
  const editorWrapperRef = useRef<HTMLDivElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const workflows = useWorkflowStore((s) => s.workflows);

  // Agent command discovery — scoped to the current agent's CLI type
  const agentProfile = useChatStore((s) => s.selectedAgentProfile);
  const agentCommandType = agentProfile?.provider?.command ?? null;
  const agentWorkingDir = defaultWorkingDirProp !== undefined ? defaultWorkingDirProp : (agentProfile?.working_dir ?? null);
  const commandsByAgent = useAgentCommandStore((s) => s.commandsByAgent);
  const agentCommands = (agentCommandType && commandsByAgent[agentCommandType]) || EMPTY_COMMANDS;
  const fetchAgentCommands = useAgentCommandStore((s) => s.fetchCommands);

  // Studio skill discovery — scoped to the current agent (RunSkill-invokable, distinct
  // from the CLI-native agentCommands above).
  const skillsByAgent = useSkillStore((s) => s.skillsByAgent);
  const skills = (agentId && skillsByAgent[agentId]) || EMPTY_SKILLS;
  const fetchSkills = useSkillStore((s) => s.fetchSkills);

  // Pending attachments state — restore from draft if available
  const [pendingAttachments, setPendingAttachments] = useState<PendingAttachment[]>(() =>
    draftAttachmentsToPending(initialDraftAttachments, agentId)
  );

  // Keep a ref to the latest plain text for cleanup callbacks. The HTML
  // (which preserves @mention pills) is read straight off the live editor at
  // save time via `editorRef` instead of being re-serialized on every
  // keystroke — see `onUpdate`.
  const messageRef = useRef(initialDraft);

  // Keep a ref to pending attachments for cleanup callbacks
  const pendingAttachmentsRef = useRef(pendingAttachments);
  pendingAttachmentsRef.current = pendingAttachments;

  // Track conversation ID in a ref so cleanup callbacks always have the correct value
  const prevConversationIdRef = useRef(conversationId);

  // Mention state driven by TipTap suggestion plugin
  const [mentionActive, setMentionActive] = useState(false);
  const [mentionQuery, setMentionQuery] = useState("");

  // Slash command state — detects `/` at position 0 of empty input
  const [slashActive, setSlashActive] = useState(false);
  const [slashQuery, setSlashQuery] = useState("");
  const [slashMenuPos, setSlashMenuPos] = useState<{ top: number; left: number } | null>(null);

  // Refs so the editor callbacks (captured at creation) can access latest slash state
  const slashActiveRef = useRef(false);
  const setSlashActiveRef = useRef(setSlashActive);
  const setSlashQueryRef = useRef(setSlashQuery);
  const setSlashMenuPosRef = useRef(setSlashMenuPos);

  // Fetch agent commands when slash popover opens
  useEffect(() => {
    if (slashActive && agentCommandType) {
      fetchAgentCommands(agentCommandType, agentWorkingDir);
    }
  }, [slashActive, agentCommandType, agentWorkingDir, fetchAgentCommands]);

  // Fetch Studio skills when slash popover opens
  useEffect(() => {
    if (slashActive && agentId) {
      fetchSkills(agentId);
    }
  }, [slashActive, agentId, fetchSkills]);

  // Store the suggestion command so MentionAutocomplete can insert mention nodes
  const suggestionCommandRef = useRef<((props: { id: string; label: string }) => void) | null>(null);

  // Ref for agentNameMap so suggestion callbacks (captured at editor creation) can access latest value
  const agentNameMapRef = useRef(agentNameMap);
  useEffect(() => { agentNameMapRef.current = agentNameMap; }, [agentNameMap]);

  // Reset cancelling state when processing ends
  useEffect(() => { if (!isProcessing) setIsCancelling(false); }, [isProcessing]);

  // Keep slash refs in sync
  useEffect(() => { slashActiveRef.current = slashActive; }, [slashActive]);

  // Resolve entityId: explicit prop, else agentId fallback
  const entityId = entityIdProp ?? agentId;

  // Refs so the handlePaste closure (captured at editor creation) can access latest values
  const fileCapabilitiesSupportedRef = useRef(fileCapabilitiesSupported);
  useEffect(() => { fileCapabilitiesSupportedRef.current = fileCapabilitiesSupported; }, [fileCapabilitiesSupported]);
  const entityIdRef = useRef(entityId);
  useEffect(() => { entityIdRef.current = entityId; }, [entityId]);
  const handleFileSelectRef = useRef<(files: FileList | null) => void>(() => { });

  // Focus mode state — persisted per store key (defaults to agentId)
  const focusKey = focusStoreKeyProp ?? agentId;
  const focusPath = useFocusPathStore((s) => focusKey ? s.focusPaths[focusKey] ?? null : null);
  const storeFocusPath = useFocusPathStore((s) => s.setFocusPath);
  const storeClearFocusPath = useFocusPathStore((s) => s.clearFocusPath);
  const setFocusPath = useCallback((path: string | null) => {
    if (!focusKey) return;
    if (path) storeFocusPath(focusKey, path);
    else storeClearFocusPath(focusKey);
  }, [focusKey, storeFocusPath, storeClearFocusPath]);

  // Derive effective focus path: explicit focus_path > agent working_dir
  const isDefaultFocusPath = !focusPath && !!agentWorkingDir;
  const effectiveFocusPath = focusPath ?? agentWorkingDir;

  // Attachment menu dropdown state
  const [showAttachMenu, setShowAttachMenu] = useState(false);
  const attachMenuRef = useRef<HTMLDivElement>(null);
  const attachButtonRef = useRef<HTMLButtonElement>(null);
  const [menuPos, setMenuPos] = useState<{ top: number; left: number } | null>(null);

  // Compute portal position from a button rect
  const openAttachMenu = useCallback((buttonEl: HTMLElement) => {
    const rect = buttonEl.getBoundingClientRect();
    const menuWidth = 208; // w-52 = 13rem = 208px
    const viewportMargin = 12;
    let left = rect.right - menuWidth; // right-aligned with button
    // Clamp to viewport bounds
    if (left + menuWidth > window.innerWidth - viewportMargin) {
      left = window.innerWidth - menuWidth - viewportMargin;
    }
    if (left < viewportMargin) {
      left = viewportMargin;
    }
    setMenuPos({
      top: rect.top - 8, // 8px gap above the button
      left,
    });
    setShowAttachMenu(true);
  }, []);

  // Close attach menu when clicking outside
  useEffect(() => {
    if (!showAttachMenu) return;
    const handleClickOutside = (e: MouseEvent) => {
      const target = e.target as Node;
      // Check if click is inside the portal menu
      if (attachMenuRef.current && attachMenuRef.current.contains(target)) return;
      // Check if click is on any paperclip button (closest traversal is more robust than ref)
      if ((target as HTMLElement).closest?.('[title="Attach files or folder"]')) return;
      setShowAttachMenu(false);
    };
    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, [showAttachMenu]);

  const hasUploading = pendingAttachments.some((a) => a.status === "uploading");
  const uploadedPending = pendingAttachments.filter((a) => a.status === "uploaded" && a.serverId);
  const uploadedIds = uploadedPending.map((a) => a.serverId!);
  const uploadedAttachments = uploadedPending
    .map((a) => a.attachment)
    .filter((a): a is Attachment => a !== null);

  const recalcHeight = useCallback(() => {
    const wrapper = editorWrapperRef.current;
    if (!wrapper) return;
    const proseMirror = wrapper.querySelector(".ProseMirror") as HTMLElement | null;
    if (!proseMirror) return;
    const newHeight = proseMirror.scrollHeight;
    setIsMultiLine(newHeight > 24);
  }, []);

  const editor = useEditor({
    extensions: [
      StarterKit.configure({
        // Only keep the basics — disable everything else
        blockquote: false,
        bulletList: false,
        codeBlock: false,
        heading: false,
        horizontalRule: false,
        listItem: false,
        orderedList: false,
        code: false,
        bold: false,
        italic: false,
        strike: false,
      }),
      Placeholder.configure({
        placeholder,
      }),
      MentionExtension.configure({
        HTMLAttributes: {
          class: "mention-chip",
        },
        suggestion: {
          char: "@",
          allowSpaces: false,
          // Return non-empty array to keep suggestion active; filtering is done in MentionAutocomplete
          items: () => [1],
          render: () => ({
            onStart: (props: { query: string; command: (attrs: { id: string; label?: string }) => void }) => {
              setMentionActive(true);
              setMentionQuery(props.query);
              suggestionCommandRef.current = (attrs) => props.command(attrs as { id: string; label?: string });
            },
            onUpdate: (props: { query: string; command: (attrs: { id: string; label?: string }) => void }) => {
              setMentionQuery(props.query);
              suggestionCommandRef.current = (attrs) => props.command(attrs as { id: string; label?: string });
            },
            onExit: () => {
              setMentionActive(false);
              setMentionQuery("");
              suggestionCommandRef.current = null;
            },
            // MentionAutocomplete handles keyboard navigation via its own capture-phase listener
            onKeyDown: () => false,
          }),
        },
      }),
    ],
    content: initialDraftHtml || (initialDraft ? `<p>${initialDraft}</p>` : ""),
    editable: true,
    editorProps: {
      attributes: {
        class:
          "flex-1 bg-transparent outline-none text-[15px] text-[var(--text-primary)] resize-none !leading-[24px] !p-0 !m-0 min-h-[24px]",
      },
      handlePaste: (_view, event) => {
        // Only intercept when agent supports attachments
        if (!fileCapabilitiesSupportedRef.current || !entityIdRef.current) return false;

        const clipboardData = event.clipboardData;
        if (!clipboardData) return false;

        // Check for image blobs in clipboard items
        const imageFiles: File[] = [];
        for (let i = 0; i < clipboardData.items.length; i++) {
          const item = clipboardData.items[i];
          if (item.type.startsWith("image/")) {
            const file = item.getAsFile();
            if (file) imageFiles.push(file);
          }
        }

        if (imageFiles.length > 0) {
          // Create a synthetic FileList-like object via DataTransfer
          const dt = new DataTransfer();
          imageFiles.forEach((f) => dt.items.add(f));
          handleFileSelectRef.current(dt.files);
          return true; // prevent default paste
        }

        // Check for pasted text that looks like an attachment-compatible file path.
        // Only intercept single-line paths ending with image/document extensions.
        // All other text (code paths, URLs, multi-line, etc.) pastes normally.
        const text = clipboardData.getData("text/plain");
        if (text) {
          const trimmed = text.trim();
          // Single line, ends with an attachment-compatible extension
          const ATTACHMENT_EXT_RE = /^[^\n]+\.(?:png|jpe?g|gif|webp|bmp|svg|pdf|doc|docx|xls|xlsx|ppt|pptx)$/i;
          if (ATTACHMENT_EXT_RE.test(trimmed)) {
            // Try to read the file — if it exists, attach it; otherwise paste as text
            exists(trimmed).then((fileExists) => {
              if (fileExists) {
                readLocalFileAsBlob(trimmed).then((file) => {
                  if (file) {
                    const dt = new DataTransfer();
                    dt.items.add(file);
                    handleFileSelectRef.current(dt.files);
                  } else {
                    _view.dispatch(_view.state.tr.insertText(trimmed));
                  }
                });
              } else {
                // File doesn't exist — insert the path as text
                _view.dispatch(_view.state.tr.insertText(trimmed));
              }
            }).catch(() => {
              _view.dispatch(_view.state.tr.insertText(trimmed));
            });
            return true; // prevent default paste while we async-check
          }
        }

        // Normal text paste — let TipTap handle it
        return false;
      },
      handleKeyDown: (_view, event) => {
        // When slash popover is active, let it handle navigation and selection keys
        if (slashActiveRef.current) {
          if (event.key === "ArrowUp" || event.key === "ArrowDown" || event.key === "Tab") {
            // SlashCommandPopover's capture listener handles these
            return true;
          }
          if (event.key === "Enter" && !event.shiftKey) {
            // SlashCommandPopover's capture listener handles Enter for selection
            return true;
          }
          if (event.key === "Escape") {
            slashActiveRef.current = false;
            setSlashActiveRef.current(false);
            setSlashQueryRef.current("");
            setSlashMenuPosRef.current(null);
            return true;
          }
        }
        if (event.key === "Enter" && !event.shiftKey) {
          event.preventDefault();
          handleSendRef.current();
          return true;
        }
        return false;
      },
    },
    onUpdate: ({ editor: ed }) => {
      const text = ed.getText();
      messageRef.current = text;
      // Keep keystrokes off this heavy component's re-render path. The counter
      // is the only thing here that tracks live length, and it's only on screen
      // near the limit — so only sync it to state when it's visible, or on the
      // one keystroke that drops it back below the threshold (to hide it).
      // Below that band, typing updates zero React state (ProseMirror owns its
      // own DOM), and the HTML is serialized lazily at save time rather than on
      // every keystroke.
      if (text.length > CHAR_COUNT_VISIBLE_AT || charCountRef.current > CHAR_COUNT_VISIBLE_AT) {
        setCharCount(text.length);
      }
      charCountRef.current = text.length;
      setHasTextContent(text.trim().length > 0);
      recalcHeight();

      // Slash command detection: `/` at position 0 of input
      if (text.startsWith("/")) {
        const query = text.slice(1); // text after the `/`
        if (!slashActiveRef.current) {
          // Compute popover anchor position from editor DOM
          const editorEl = ed.view.dom;
          if (editorEl) {
            const rect = editorEl.getBoundingClientRect();
            setSlashMenuPosRef.current({ top: rect.top, left: rect.left });
          }
        }
        slashActiveRef.current = true;
        setSlashActiveRef.current(true);
        setSlashQueryRef.current(query);
      } else if (slashActiveRef.current) {
        // Text no longer starts with `/` — clear slash state
        slashActiveRef.current = false;
        setSlashActiveRef.current(false);
        setSlashQueryRef.current("");
        setSlashMenuPosRef.current(null);
      }
    },
  });

  // Always-current handle to the live editor. The unmount / conversation-reset
  // callbacks run from []-deps effects that would otherwise close over a stale
  // (initially null) editor, which is why HTML used to be mirrored into a ref
  // on every keystroke. Reading through this ref lets us serialize the draft
  // HTML lazily — only when we actually save — instead of on every keystroke.
  const editorRef = useRef(editor);
  editorRef.current = editor;

  // Stable ref for handleSend so the editorProps closure can call it
  const handleSendRef = useRef(() => { });

  const handleSend = useCallback(() => {
    if (!editor) return;
    if (hasUploading) return; // block send while uploads in progress
    const trimmed = editor.getText().trim();
    if (!trimmed && uploadedIds.length === 0) return;
    if (trimmed.length > MAX_CHARS) {
      setBorderFlash(true);
      setTimeout(() => setBorderFlash(false), 600);
      return;
    }
    if (disabled) {
      // Flash the border red twice to signal that sending is blocked
      setBorderFlash(true);
      setTimeout(() => setBorderFlash(false), 600);
      return;
    }
    onSend(
      trimmed,
      uploadedIds.length > 0 ? uploadedIds : undefined,
      uploadedAttachments.length > 0 ? uploadedAttachments : undefined,
    );
    editor.commands.clearContent();
    messageRef.current = "";
    charCountRef.current = 0;
    setCharCount(0); // no-op re-render when already 0 (the common case)
    setHasTextContent(false);
    setPendingAttachments([]);
    requestAnimationFrame(() => {
      recalcHeight();
      editor.commands.focus();
    });
  }, [editor, disabled, onSend, recalcHeight, hasUploading, uploadedIds, uploadedAttachments]);

  handleSendRef.current = handleSend;

  // Handle file selection from the file picker
  const handleFileSelect = useCallback(
    async (files: FileList | null) => {
      if (!files || files.length === 0 || !entityId) return;

      const newPending: PendingAttachment[] = Array.from(files).map((file) => ({
        id: `pending-${Date.now()}-${Math.random().toString(36).slice(2, 9)}`,
        file,
        previewUrl: file.type.startsWith("image/") ? URL.createObjectURL(file) : null,
        status: "uploading" as const,
        serverId: null,
        attachment: null,
        isFolder: false,
      }));

      setPendingAttachments((prev) => [...prev, ...newPending]);

      // Upload each file
      for (const pending of newPending) {
        try {
          const attachment = onUploadAttachment
            ? await onUploadAttachment(pending.file!)
            : await api.uploadAttachment(agentId!, pending.file!);
          setPendingAttachments((prev) =>
            prev.map((p) =>
              p.id === pending.id
                ? { ...p, status: "uploaded" as const, serverId: attachment.id, attachment }
                : p
            )
          );
        } catch {
          setPendingAttachments((prev) =>
            prev.map((p) =>
              p.id === pending.id ? { ...p, status: "error" as const } : p
            )
          );
        }
      }
    },
    [entityId, agentId, onUploadAttachment]
  );

  // Keep ref updated for paste handler closure
  handleFileSelectRef.current = handleFileSelect;

  // Handle folder selection via Tauri dialog
  const handleFolderSelect = useCallback(async () => {
    if (!entityId) return;
    try {
      const selected = await tauriOpen({ directory: true, multiple: false });
      if (!selected) return;

      const folderPath = selected as string;
      const folderName = folderPath.split("/").pop() || folderPath;
      const pendingId = `pending-${Date.now()}-${Math.random().toString(36).slice(2, 9)}`;

      const pending: PendingAttachment = {
        id: pendingId,
        file: new window.File([], folderName),
        previewUrl: null,
        status: "uploading",
        serverId: null,
        attachment: null,
        isFolder: true,
        folderPath,
      };
      setPendingAttachments((prev) => [...prev, pending]);

      try {
        const attachment = onAddFolderReference
          ? await onAddFolderReference(folderPath)
          : await api.addFolderReference(agentId!, folderPath);
        setPendingAttachments((prev) =>
          prev.map((p) =>
            p.id === pendingId
              ? { ...p, status: "uploaded" as const, serverId: attachment.id, attachment }
              : p
          )
        );
      } catch {
        setPendingAttachments((prev) =>
          prev.map((p) =>
            p.id === pendingId ? { ...p, status: "error" as const } : p
          )
        );
      }
    } catch {
      // Dialog was cancelled or failed — no-op
    }
  }, [entityId, agentId, onAddFolderReference]);

  // Remove a pending attachment
  const handleRemoveAttachment = useCallback(
    async (pendingId: string) => {
      const pending = pendingAttachments.find((p) => p.id === pendingId);
      if (!pending) return;

      // Clean up preview URL
      if (pending.previewUrl) {
        URL.revokeObjectURL(pending.previewUrl);
      }

      // Drop any preserved in-flight upload File for this pending ID
      uploadingFilesRegistry.delete(pendingId);

      // Delete from server if already uploaded
      if (pending.status === "uploaded" && pending.serverId && entityId) {
        try {
          if (onDeleteAttachment) {
            await onDeleteAttachment(pending.serverId);
          } else if (agentId) {
            await api.deleteAttachment(agentId, pending.serverId);
          }
        } catch {
          // Best effort — still remove from UI
        }
      }

      setPendingAttachments((prev) => prev.filter((p) => p.id !== pendingId));
    },
    [pendingAttachments, entityId, agentId, onDeleteAttachment]
  );

  // Verify that server-side attachments restored from a saved draft still
  // exist. The server cleans up uncommitted attachments after 1 hour, so a
  // draft left open past that window will have dangling attachment IDs. When
  // the info endpoint returns 404 we flip the entry to status="expired" so
  // the UI can show a clear fallback instead of a broken image.
  const verifiedRestoredRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    if (!agentId) return;
    for (const p of pendingAttachments) {
      if (
        p.status !== "uploaded"
        || !p.serverId
        || !p.id.startsWith("restored-")
        || verifiedRestoredRef.current.has(p.id)
      ) continue;
      verifiedRestoredRef.current.add(p.id);
      const pending = p;
      (async () => {
        const exists = await api.verifyAttachmentExists(agentId, pending.serverId!);
        if (exists === false) {
          setPendingAttachments((prev) =>
            prev.map((q) => (q.id === pending.id ? { ...q, status: "expired" as const } : q))
          );
        }
      })();
    }
  }, [pendingAttachments, agentId]);

  // Re-upload attachments that were restored in "uploading" state. These are
  // entries whose pending ID still lives in the module-level upload registry
  // (meaning the File was preserved across the conversation switch).
  const initiatedReuploadsRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    if (!agentId) return;
    for (const p of pendingAttachments) {
      if (
        p.status !== "uploading"
        || p.serverId !== null
        || !p.file
        || p.file.size === 0
        || !uploadingFilesRegistry.has(p.id)
        || initiatedReuploadsRef.current.has(p.id)
      ) continue;
      initiatedReuploadsRef.current.add(p.id);
      const pending = p;
      (async () => {
        try {
          const attachment = onUploadAttachment
            ? await onUploadAttachment(pending.file!)
            : await api.uploadAttachment(agentId, pending.file!);
          setPendingAttachments((prev) =>
            prev.map((q) =>
              q.id === pending.id
                ? { ...q, status: "uploaded" as const, serverId: attachment.id, attachment }
                : q
            )
          );
          uploadingFilesRegistry.delete(pending.id);
        } catch {
          setPendingAttachments((prev) =>
            prev.map((q) => (q.id === pending.id ? { ...q, status: "error" as const } : q))
          );
        }
      })();
    }
  }, [pendingAttachments, agentId, onUploadAttachment]);

  // Drag-and-drop handlers
  const [dropRejectMessage, setDropRejectMessage] = useState<string | null>(null);
  const dropRejectTimerRef = useRef<ReturnType<typeof setTimeout>>(null);

  const handleDragEnter = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (!fileCapabilitiesSupported || !entityId) return;
      dragCounterRef.current += 1;
      if (dragCounterRef.current === 1) {
        setIsDragOver(true);
      }
    },
    [fileCapabilitiesSupported, entityId]
  );

  const handleDragOver = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
    },
    []
  );

  const handleDragLeave = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
      dragCounterRef.current -= 1;
      if (dragCounterRef.current <= 0) {
        dragCounterRef.current = 0;
        setIsDragOver(false);
      }
    },
    []
  );

  const handleDrop = useCallback(
    async (e: React.DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
      dragCounterRef.current = 0;
      setIsDragOver(false);

      if (!fileCapabilitiesSupported || !entityId) return;

      const items = e.dataTransfer.items;
      const filesToUpload: File[] = [];
      const foldersToReference: string[] = [];
      const rejected: string[] = [];

      // Check each item for directories vs files
      for (let i = 0; i < items.length; i++) {
        const item = items[i];
        const entry = item.webkitGetAsEntry?.();

        if (entry?.isDirectory) {
          // Folder — use folder reference API
          foldersToReference.push(entry.name);
          continue;
        }

        const file = item.getAsFile();
        if (!file) continue;

        // Validate MIME type against accepted types
        const isAllowed = ACCEPTED_MIME_TYPES.split(",").some((accepted) => {
          const trimmed = accepted.trim();
          if (trimmed.endsWith("/*")) {
            return file.type.startsWith(trimmed.replace("/*", "/"));
          }
          return file.type === trimmed;
        });

        if (isAllowed) {
          filesToUpload.push(file);
        } else {
          rejected.push(file.name);
        }
      }

      // Show rejection message for unsupported files
      if (rejected.length > 0) {
        const msg = rejected.length === 1
          ? `"${rejected[0]}" is not a supported file type`
          : `${rejected.length} files have unsupported types`;
        if (dropRejectTimerRef.current) clearTimeout(dropRejectTimerRef.current);
        setDropRejectMessage(msg);
        dropRejectTimerRef.current = setTimeout(() => setDropRejectMessage(null), 3000);
      }

      // Handle folder references
      for (const folderName of foldersToReference) {
        const pendingId = `pending-${Date.now()}-${Math.random().toString(36).slice(2, 9)}`;
        const pending: PendingAttachment = {
          id: pendingId,
          file: null,
          previewUrl: null,
          status: "uploading",
          serverId: null,
          attachment: null,
          isFolder: true,
        };
        setPendingAttachments((prev) => [...prev, { ...pending, file: new window.File([], folderName) }]);

        try {
          const attachment = onAddFolderReference
            ? await onAddFolderReference(folderName)
            : await api.addFolderReference(agentId!, folderName);
          setPendingAttachments((prev) =>
            prev.map((p) =>
              p.id === pendingId
                ? { ...p, status: "uploaded" as const, serverId: attachment.id, attachment }
                : p
            )
          );
        } catch {
          setPendingAttachments((prev) =>
            prev.map((p) =>
              p.id === pendingId ? { ...p, status: "error" as const } : p
            )
          );
        }
      }

      // Handle file uploads — reuse handleFileSelect
      if (filesToUpload.length > 0) {
        const dt = new DataTransfer();
        filesToUpload.forEach((f) => dt.items.add(f));
        handleFileSelect(dt.files);
      }
    },
    [fileCapabilitiesSupported, entityId, agentId, onAddFolderReference, handleFileSelect]
  );

  // Handle mention selection via TipTap suggestion command
  const handleMentionSelect = useCallback(
    (agentIdParam: string) => {
      const label = agentNameMapRef.current?.[agentIdParam] ?? agentIdParam;
      if (suggestionCommandRef.current) {
        suggestionCommandRef.current({ id: agentIdParam, label });
      }
    },
    []
  );

  // Update placeholder when prop changes
  useEffect(() => {
    if (editor) {
      editor.extensionManager.extensions.forEach((ext) => {
        if (ext.name === "placeholder") {
          (ext.options as Record<string, unknown>).placeholder = placeholder;
          editor.view.dispatch(editor.state.tr);
        }
      });
    }
  }, [editor, placeholder]);

  // Autofocus on mount
  useEffect(() => {
    if (editor) {
      editor.commands.focus();
      recalcHeight();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editor]);

  // Save draft on unmount
  useEffect(() => {
    return () => {
      const convId = prevConversationIdRef.current ?? "";
      onUnmount?.(messageRef.current, editorRef.current?.getHTML() ?? "", convId);
      onUnmountAttachments?.(pendingToDraftAttachments(pendingAttachmentsRef.current), convId);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Reset editor when conversationId changes
  useEffect(() => {
    if (prevConversationIdRef.current !== conversationId) {
      const prevId = prevConversationIdRef.current ?? "";
      onUnmount?.(messageRef.current, editorRef.current?.getHTML() ?? "", prevId);
      onUnmountAttachments?.(pendingToDraftAttachments(pendingAttachmentsRef.current), prevId);
      prevConversationIdRef.current = conversationId;
      if (editor) {
        // Restore from HTML if available (preserves @mention pills), otherwise plain text
        const htmlContent = initialDraftHtml || (initialDraft ? `<p>${initialDraft}</p>` : "");
        editor.commands.setContent(htmlContent);
        messageRef.current = initialDraft;
        // Re-seed the counter for the new conversation's draft. It stays out of
        // state (0) unless the restored draft is already near the limit.
        charCountRef.current = initialDraft.length;
        setCharCount(initialDraft.length > CHAR_COUNT_VISIBLE_AT ? initialDraft.length : 0);
        setMentionActive(false);
        setMentionQuery("");
        // Restore draft attachments for the new conversation. Reset the
        // re-upload tracking set so any restored "uploading" attachments
        // trigger a fresh upload via the reupload effect, and the
        // verification set so restored "uploaded" entries get re-checked
        // against the server.
        initiatedReuploadsRef.current = new Set();
        verifiedRestoredRef.current = new Set();
        setPendingAttachments(draftAttachmentsToPending(initialDraftAttachments, agentId));
        setHasTextContent(!!initialDraft);
        requestAnimationFrame(() => {
          editor.commands.focus();
          recalcHeight();
        });
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [conversationId]);

  // Determine if icons should be outside the input
  const showSendButton = hasTextContent || pendingAttachments.length > 0;
  const showExternalPaperclip = fileCapabilitiesSupported && entityId && showSendButton;
  const showInternalPaperclip = fileCapabilitiesSupported && entityId && !showSendButton;

  return (
    <div
      style={{ position: "relative" }}
      onDragEnter={handleDragEnter}
      onDragOver={handleDragOver}
      onDragLeave={handleDragLeave}
      onDrop={handleDrop}
    >
      {/* Drop overlay indicator */}
      <AnimatePresence>
        {isDragOver && fileCapabilitiesSupported && entityId && (
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
            className="absolute inset-0 z-20 flex items-center justify-center rounded-2xl border-2 border-dashed border-[var(--input-focus-border)] bg-[var(--bg-secondary)]/60 backdrop-blur-sm pointer-events-none"
          >
            <span className="text-sm font-medium text-[var(--text-secondary)]">Drop files here</span>
          </motion.div>
        )}
      </AnimatePresence>

      {/* Rejection toast */}
      <AnimatePresence>
        {dropRejectMessage && (
          <motion.div
            initial={{ opacity: 0, y: 8 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: 8 }}
            transition={{ duration: 0.2 }}
            className="absolute -top-10 left-1/2 -translate-x-1/2 z-30 px-3 py-1.5 rounded-lg bg-red-500/90 text-white text-xs whitespace-nowrap shadow-lg"
          >
            {dropRejectMessage}
          </motion.div>
        )}
      </AnimatePresence>

      {teamMembers && teamMembers.length > 0 && (
        <MentionAutocomplete
          members={teamMembers}
          query={mentionQuery}
          visible={mentionActive}
          onSelect={handleMentionSelect}
          onClose={() => setMentionActive(false)}
          agentNameMap={agentNameMap}
        />
      )}

      <SlashCommandPopover
        workflows={workflows}
        agentCommands={agentCommands}
        skills={skills}
        query={slashQuery}
        visible={slashActive && slashMenuPos !== null}
        onSelect={(workflow) => {
          // Clear editor content and slash state
          editor?.commands.clearContent();
          setSlashActive(false);
          setSlashQuery("");
          setSlashMenuPos(null);
          slashActiveRef.current = false;
          // Open task creation modal with selected workflow
          useTaskCreateModalStore.getState().open(workflow.id);
        }}
        onSelectCommand={(cmd) => {
          // Replace editor content with /<slug> so user can add arguments
          editor?.commands.setContent(`/${cmd.slug} `);
          // Move cursor to end
          editor?.commands.focus("end");
          // Close popover
          setSlashActive(false);
          setSlashQuery("");
          setSlashMenuPos(null);
          slashActiveRef.current = false;
        }}
        onSelectSkill={(skill) => {
          // Same shape as onSelectCommand: insert literal `/<slug> ` so the
          // model's system-prompt instruction (see render_studio_skills_block)
          // can recognize it and call RunSkill.
          editor?.commands.setContent(`/${skill.id} `);
          // Move cursor to end
          editor?.commands.focus("end");
          // Close popover
          setSlashActive(false);
          setSlashQuery("");
          setSlashMenuPos(null);
          slashActiveRef.current = false;
        }}
        onClose={() => {
          setSlashActive(false);
          setSlashQuery("");
          setSlashMenuPos(null);
          slashActiveRef.current = false;
        }}
      />

      {/* Hidden file input */}
      {fileCapabilitiesSupported && entityId && (
        <input
          ref={fileInputRef}
          type="file"
          multiple
          accept={ACCEPTED_MIME_TYPES}
          className="hidden"
          onChange={(e) => {
            handleFileSelect(e.target.files);
            e.target.value = "";
          }}
        />
      )}

      <div className={twMerge("flex gap-2", isMultiLine || pendingAttachments.length > 0 ? "items-end" : "items-center")}>
        {/* Input column — input box + focus strip, aligned together */}
        <div className="flex-1 flex flex-col min-w-0">
          {/* Input container with shadow — contains pills, editor, paperclip/send */}
          <motion.div
            ref={containerRef}
            layout
            className={twMerge(
              "flex flex-1 flex-col bg-[var(--chat-input-bg)] border border-[var(--border-primary)] rounded-[14px] min-h-[50px] max-h-[350px] shadow-sm focus-within:border-[var(--input-focus-border)] focus-within:shadow-md overflow-hidden cursor-text",
              pendingAttachments.length === 0 && !isMultiLine && "justify-center",
              borderFlash && "animate-border-flash"
            )}
            transition={{ layout: { duration: 0.15, ease: "easeOut" } }}
            onClick={(e) => {
              // Focus the editor when clicking anywhere in the container except interactive elements
              const target = e.target as HTMLElement;
              if (target.closest("button, img, a, input, [role='button']")) return;
              editor?.commands.focus();
            }}
          >
            {/* Pending attachments strip — inside the shadow div, like mentions */}
            <AnimatePresence>
              {pendingAttachments.length > 0 && (
                <motion.div
                  initial={{ opacity: 0, height: 0 }}
                  animate={{ opacity: 1, height: "auto" }}
                  exit={{ opacity: 0, height: 0 }}
                  transition={{ duration: 0.25, ease: "easeOut" }}
                  className="overflow-hidden flex-shrink-0"
                >
                  <div className="flex gap-1 px-2.5 py-2 overflow-x-auto mx-1.5 mt-1.5 rounded-xl" style={{ scrollbarWidth: "thin" }}>
                    <AnimatePresence initial={false}>
                      {pendingAttachments.map((pa) => {
                        const effectiveType = getEffectiveType(pa);
                        const isImage = effectiveType === "image" && pa.previewUrl;

                        const statusTooltip = pa.status === "error"
                          ? "Upload failed — click X to remove and retry"
                          : pa.status === "expired"
                            ? `Attachment expired: ${pa.file?.name ?? pa.attachment?.original_filename ?? "file"}`
                            : null;

                        return isImage ? (
                          <FileTooltip label={statusTooltip ?? (pa.file?.name ?? "image")}>
                            <motion.div
                              key={pa.id}
                              initial={{ opacity: 0, scale: 0.85, filter: "blur(4px)" }}
                              animate={{ opacity: 1, scale: 1, filter: "blur(0px)" }}
                              exit={{ opacity: 0, scale: 0.9, filter: "blur(4px)" }}
                              transition={{ duration: 0.2, ease: "easeOut" }}
                              className="relative flex-shrink-0 w-[56px] group cursor-pointer"
                            >
                              {/* Image thumbnail */}
                              <div className={twMerge(
                                "w-[44px] h-[44px] mx-auto rounded-lg overflow-hidden ring-1 ring-black/10 dark:ring-white/15 shadow-sm",
                                pa.status === "error" && "!ring-red-500/60",
                                pa.status === "expired" && "!ring-amber-500/60 bg-[var(--bg-secondary)]"
                              )}>
                                {pa.status === "expired" ? (
                                  <div className="w-full h-full flex items-center justify-center">
                                    <ImageOff size={20} className="text-[var(--text-tertiary)]" />
                                  </div>
                                ) : (
                                  <DraftImageThumbnail
                                    src={pa.previewUrl!}
                                    alt={pa.file?.name ?? ""}
                                    onClick={() => {
                                      useMediaPreviewStore.getState().openPreview({
                                        content: pa.previewUrl!,
                                        contentType: "image",
                                        filename: pa.file?.name,
                                      });
                                    }}
                                  />
                                )}
                              </div>

                              {/* Truncated filename */}
                              <div className={twMerge(
                                "w-full text-[10px] text-center truncate mt-[2px] px-[1px] leading-tight",
                                pa.status === "expired"
                                  ? "text-amber-600 dark:text-amber-400 opacity-90"
                                  : "text-[var(--text-primary)] opacity-70"
                              )}>
                                {pa.status === "expired" ? "expired" : (pa.file?.name ?? "image")}
                              </div>

                              {/* Upload spinner overlay */}
                              {pa.status === "uploading" && (
                                <div className="absolute top-0 left-1/2 -translate-x-1/2 w-[44px] h-[44px] flex items-center justify-center bg-black/40 rounded-lg">
                                  <Loader2 size={16} className="animate-spin text-white" />
                                </div>
                              )}

                              {/* Error icon overlay */}
                              {pa.status === "error" && (
                                <div className="absolute top-0 left-1/2 -translate-x-1/2 w-[44px] h-[44px] flex items-center justify-center bg-red-500/20 rounded-lg">
                                  <AlertCircle size={16} className="text-red-500" />
                                </div>
                              )}

                              {/* Remove button */}
                              <button
                                onClick={(e) => { e.stopPropagation(); handleRemoveAttachment(pa.id); }}
                                className="absolute top-[-4px] right-[2px] w-4 h-4 flex items-center justify-center rounded-full bg-[var(--bg-primary)] border border-[var(--border-primary)] opacity-0 group-hover:opacity-100 transition-opacity z-10"
                                title="Remove attachment"
                              >
                                <X size={8} className="text-[var(--text-secondary)]" />
                              </button>
                            </motion.div>
                          </FileTooltip>
                        ) : (
                          <FileTooltip label={statusTooltip ?? (pa.folderPath ?? pa.file?.name ?? "file")}>
                            <motion.div
                              key={pa.id}
                              initial={{ opacity: 0, scale: 0.85, filter: "blur(4px)" }}
                              animate={{ opacity: 1, scale: 1, filter: "blur(0px)" }}
                              exit={{ opacity: 0, scale: 0.9, filter: "blur(4px)" }}
                              transition={{ duration: 0.2, ease: "easeOut" }}
                              className="relative flex-shrink-0 w-[56px] group cursor-default"
                            >
                              {/* Icon tile */}
                              <div className={twMerge(
                                "w-[44px] h-[44px] mx-auto",
                                pa.status === "error" && "ring-1 ring-red-500/60 rounded-lg",
                                pa.status === "expired" && "ring-1 ring-amber-500/60 rounded-lg bg-[var(--bg-secondary)] flex items-center justify-center"
                              )}>
                                {pa.status === "expired" ? (
                                  <ImageOff size={20} className="text-[var(--text-tertiary)]" />
                                ) : (
                                  <FileIcon
                                    fileName={pa.file?.name ?? "file"}
                                    fileType={effectiveType}
                                  />
                                )}
                              </div>

                              {/* Truncated filename */}
                              <div className={twMerge(
                                "w-full text-[10px] text-center truncate mt-[2px] px-[1px] leading-tight",
                                pa.status === "expired"
                                  ? "text-amber-600 dark:text-amber-400 opacity-90"
                                  : "text-[var(--text-primary)] opacity-70"
                              )}>
                                {pa.status === "expired" ? "expired" : (pa.file?.name ?? "file")}
                              </div>

                              {/* Upload spinner overlay */}
                              {pa.status === "uploading" && (
                                <div className="absolute top-0 left-1/2 -translate-x-1/2 w-[44px] h-[44px] flex items-center justify-center bg-black/40 rounded-lg">
                                  <Loader2 size={16} className="animate-spin text-white" />
                                </div>
                              )}

                              {/* Error icon overlay */}
                              {pa.status === "error" && (
                                <div className="absolute top-0 left-1/2 -translate-x-1/2 w-[44px] h-[44px] flex items-center justify-center bg-red-500/20 rounded-lg">
                                  <AlertCircle size={16} className="text-red-500" />
                                </div>
                              )}

                              {/* Remove button */}
                              <button
                                onClick={() => handleRemoveAttachment(pa.id)}
                                className="absolute top-[-4px] right-[2px] w-4 h-4 flex items-center justify-center rounded-full bg-[var(--bg-primary)] border border-[var(--border-primary)] opacity-0 group-hover:opacity-100 transition-opacity z-10"
                                title="Remove attachment"
                              >
                                <X size={8} className="text-[var(--text-secondary)]" />
                              </button>
                            </motion.div>
                          </FileTooltip>
                        );
                      })}
                    </AnimatePresence>
                  </div>
                </motion.div>
              )}
            </AnimatePresence>

            {/* Editor row — chevron, editor, and right-side action button */}
            <div className={twMerge(
              "flex px-[16px] py-[10px] gap-2",
              isMultiLine ? "items-end" : "items-center"
            )}>
              <div
                className={twMerge(
                  "flex-shrink-0 w-[20px] flex items-center justify-center transition-all duration-300",
                  isMultiLine ? "self-stretch py-1" : "h-[24px]"
                )}
              >
                <svg
                  width="20"
                  height={isMultiLine ? "100%" : "20"}
                  viewBox="0 0 20 20"
                  preserveAspectRatio={isMultiLine ? "none" : "xMidYMid meet"}
                  fill="none"
                  className="overflow-visible"
                >
                  <motion.path
                    initial={{ pathLength: 0, opacity: 0, d: "M 8 5 L 13 10 L 8 15" }}
                    animate={{
                      pathLength: 1,
                      opacity: 1,
                      d: isMultiLine
                        ? "M 10 0 L 10 10 L 10 20"
                        : "M 8 5 L 13 10 L 8 15",
                      stroke: isMultiLine ? "var(--border-secondary)" : "var(--text-secondary)",
                    }}
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    transition={{
                      d: { duration: 0.5, ease: [0.4, 0, 0.2, 1] },
                      pathLength: { duration: 0.8, ease: "easeInOut" },
                      opacity: { duration: 0.3 },
                    }}
                  />
                </svg>
              </div>
              <div ref={editorWrapperRef} className={twMerge("relative flex-1 min-h-[24px] overflow-y-auto", pendingAttachments.length > 0 ? "max-h-[150px]" : "max-h-[250px]")}>
                <EditorContent editor={editor} />
              </div>

              {/* Character limit indicator */}
              {charCount > MAX_CHARS * 0.8 && (
                <span className={`flex-shrink-0 text-[11px] tabular-nums mr-[4px] ${charCount > MAX_CHARS ? "text-red-500 font-medium" : "text-[var(--text-tertiary)]"}`}>
                  {charCount.toLocaleString()}/{(MAX_CHARS / 1000).toFixed(0)}k
                </span>
              )}

              {/* Right-side action: paperclip when empty, send when has content, stop when processing */}
              <AnimatePresence mode="popLayout">
                {isProcessing && onStop ? (
                  <motion.button
                    key="stop-button"
                    initial={{ opacity: 0, scale: 0.8 }}
                    animate={{ opacity: 1, scale: 1 }}
                    exit={{ opacity: 0, scale: 0.8 }}
                    transition={{ duration: 0.15 }}
                    onClick={() => { if (!isCancelling) { setIsCancelling(true); onStop(); } }}
                    disabled={isCancelling}
                    className={twMerge(
                      "flex-shrink-0 w-[24px] h-[24px] flex items-center justify-center rounded-full transition-colors",
                      isCancelling
                        ? "bg-[var(--bg-tertiary)] cursor-not-allowed"
                        : "bg-red-500/15 hover:bg-red-500/25 cursor-pointer"
                    )}
                    title={isCancelling ? "Cancelling..." : "Stop response"}
                  >
                    {isCancelling ? (
                      <Loader2 size={12} className="animate-spin text-[var(--text-tertiary)]" />
                    ) : (
                      <div className="w-[8px] h-[8px] rounded-[2px] bg-red-500" />
                    )}
                  </motion.button>
                ) : showSendButton ? (
                  <motion.button
                    key="send-button"
                    initial={{ opacity: 0, scale: 0.5 }}
                    animate={{ opacity: 1, scale: 1, transition: { type: "spring", stiffness: 500, damping: 35, mass: 0.8 } }}
                    exit={{ opacity: 0, scale: 0.8, transition: { duration: 0.1, ease: "easeOut" } }}
                    onClick={handleSend}
                    disabled={hasUploading}
                    className={twMerge(
                      "flex-shrink-0 w-[24px] h-[24px] flex items-center justify-center rounded-lg transition-colors cursor-pointer",
                      hasUploading
                        ? "bg-[var(--bg-tertiary)] text-[var(--text-tertiary)] cursor-not-allowed"
                        : "bg-[var(--text-primary)] text-[var(--bg-primary)] hover:opacity-90"
                    )}
                    title={hasUploading ? "Waiting for uploads..." : "Send message"}
                  >
                    {hasUploading ? (
                      <Loader2 size={16} className="animate-spin" />
                    ) : (
                      <ArrowUp size={14} strokeWidth={2.5} />
                    )}
                  </motion.button>
                ) : showInternalPaperclip ? (
                  <motion.button
                    key="paperclip-internal"
                    ref={attachButtonRef}
                    initial={{ opacity: 0, scale: 0.8 }}
                    animate={{ opacity: 1, scale: 1 }}
                    exit={{ opacity: 0, scale: 0.8 }}
                    transition={{ duration: 0.15 }}
                    onClick={(e) => showAttachMenu ? setShowAttachMenu(false) : openAttachMenu(e.currentTarget)}
                    className="flex-shrink-0 w-[24px] h-[24px] flex items-center justify-center rounded-md hover:bg-[var(--bg-tertiary)] transition-colors cursor-pointer"
                    title="Attach files or folder"
                  >
                    <Paperclip size={14} className="text-[var(--text-tertiary)]" />
                  </motion.button>
                ) : null}
              </AnimatePresence>
            </div>
          </motion.div>

          {/* Focus mode strip — aligned with the input box */}
          <AnimatePresence>
            {effectiveFocusPath && (
              <motion.div
                initial={{ opacity: 0, height: 0, marginTop: 0 }}
                animate={{ opacity: 1, height: "auto", marginTop: 6 }}
                exit={{ opacity: 0, height: 0, marginTop: 0 }}
                transition={{ duration: 0.2, ease: "easeOut" }}
                className="overflow-hidden"
              >
                <div className="flex items-center px-3 py-[2px] max-w-full overflow-hidden">
                  <span className="font-bold text-xs uppercase flex-shrink-0 whitespace-nowrap text-[var(--text-primary)]">Focus mode</span>
                  <ChevronRight size={15} className="flex-shrink-0 text-[var(--text-primary)]" />
                  <span
                    className="text-sm text-[var(--text-secondary)] flex-1 min-w-0 truncate"
                    title={effectiveFocusPath}
                  >
                    {effectiveFocusPath}
                  </span>
                  {isDefaultFocusPath && (
                    <span className="text-xs text-[var(--text-tertiary)] ml-1 flex-shrink-0 whitespace-nowrap">(default)</span>
                  )}
                  {!isDefaultFocusPath && (
                    <span
                      onClick={() => setFocusPath(null)}
                      className="flex-shrink-0 text-xs text-[var(--text-secondary)] cursor-pointer hover:text-[var(--text-primary)] hover:underline ml-2"
                    >
                      Remove focus
                    </span>
                  )}
                </div>
              </motion.div>
            )}
          </AnimatePresence>
        </div>

        {/* External paperclip — slides out to the right when there's content */}
        <AnimatePresence>
          {showExternalPaperclip && (
            <motion.button
              key="paperclip-external"
              ref={attachButtonRef}
              initial={{ opacity: 0, scale: 0.5, width: 0, minWidth: 0 }}
              animate={{ opacity: 1, scale: 1, width: 32, minWidth: 32, transition: { type: "spring", stiffness: 500, damping: 35, mass: 0.8 } }}
              exit={{ opacity: 0, scale: 0.5, width: 0, minWidth: 0, transition: { duration: 0.12, ease: "easeOut" } }}
              onClick={(e) => showAttachMenu ? setShowAttachMenu(false) : openAttachMenu(e.currentTarget)}
              className={twMerge(
                "flex-shrink-0 min-w-[32px] min-h-[32px] w-[32px] h-[32px] flex items-center justify-center rounded-lg hover:bg-[var(--bg-tertiary)] transition-colors cursor-pointer",
                (isMultiLine || pendingAttachments.length > 0) && "mb-[9px]",
                effectiveFocusPath && (isMultiLine || pendingAttachments.length > 0) && "mb-[35px]",
                effectiveFocusPath && !(isMultiLine || pendingAttachments.length > 0) && "mb-[26px]"
              )}
              title="Attach files or folder"
            >
              <Paperclip size={16} className="text-[var(--text-tertiary)]" />
            </motion.button>
          )}
        </AnimatePresence>
      </div>

      {/* Attach menu rendered as a portal so it escapes overflow-hidden */}
      {showAttachMenu && menuPos && createPortal(
        <div
          ref={attachMenuRef}
          className="chat-attach-popover fixed z-50 w-52 rounded-xl border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] shadow-lg p-1.5"
          style={{ top: menuPos.top, left: menuPos.left, transform: "translateY(-100%)" }}
        >
          <button
            type="button"
            onClick={() => { setShowAttachMenu(false); fileInputRef.current?.click(); }}
            className="flex items-center gap-2.5 w-full px-3 py-2.5 text-sm text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] rounded-lg transition-colors cursor-pointer"
          >
            <File size={16} className="text-[var(--modal-text-secondary)]" />
            Files
          </button>
          <button
            type="button"
            onClick={() => { setShowAttachMenu(false); handleFolderSelect(); }}
            className="flex items-center gap-2.5 w-full px-3 py-2.5 text-sm text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] rounded-lg transition-colors cursor-pointer"
          >
            <FolderOpen size={16} className="text-[var(--modal-text-secondary)]" />
            Folder
          </button>
          <div className="h-px bg-[var(--modal-border-primary)] mx-2 my-1" />
          <button
            type="button"
            onClick={async () => {
              setShowAttachMenu(false);
              try {
                const selected = await tauriOpen({ directory: true, multiple: false });
                if (selected) {
                  setFocusPath(selected as string);
                }
              } catch {
                // Dialog cancelled
              }
            }}
            className="flex items-center gap-2.5 w-full px-3 py-2.5 text-sm text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] rounded-lg transition-colors cursor-pointer"
          >
            <Focus size={16} className="text-[var(--modal-text-secondary)]" />
            <div className="flex flex-col items-start">
              <span>Focus mode</span>
              <span className="text-[10px] text-[var(--modal-text-tertiary)] leading-tight">Focus work within folder</span>
            </div>
          </button>
        </div>,
        document.body
      )}

    </div>
  );
}
