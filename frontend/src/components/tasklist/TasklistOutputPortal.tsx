import { lazy, Suspense, useEffect, useMemo, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { X, Loader2, FileText, Copy, Check, Download } from "lucide-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { save } from "@tauri-apps/plugin-dialog";
import { writeFile } from "@tauri-apps/plugin-fs";
import { useTasklistOutputStore } from "../../stores/tasklistOutputStore";
import * as api from "../../lib/api";
import { displayOutputFilename } from "../../lib/expectedOutputs";
import { Tooltip } from "../ui/Tooltip";
import type { TasklistScope } from "../../types/api";

const LazyRichMarkdown = lazy(() =>
  import("../shared/RichMarkdown").then((mod) => ({ default: mod.RichMarkdown })),
);

const MARKDOWN_EXTS = new Set([".md", ".markdown", ".mdx"]);
const IMAGE_EXTS = new Set([".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".bmp", ".ico", ".avif"]);
const PDF_EXTS = new Set([".pdf"]);
const HTML_EXTS = new Set([".html", ".htm"]);
const VIDEO_EXTS = new Set([".mp4", ".webm", ".mov", ".m4v"]);
const AUDIO_EXTS = new Set([".mp3", ".wav", ".ogg", ".m4a", ".flac"]);

type RenderKind = "image" | "pdf" | "html" | "video" | "audio" | "markdown" | "text" | "code";

function fileExt(filename: string): string {
  const dot = filename.lastIndexOf(".");
  return dot >= 0 ? filename.slice(dot).toLowerCase() : "";
}

function classifyFile(filename: string): RenderKind {
  const ext = fileExt(filename);
  if (IMAGE_EXTS.has(ext)) return "image";
  if (PDF_EXTS.has(ext)) return "pdf";
  if (HTML_EXTS.has(ext)) return "html";
  if (VIDEO_EXTS.has(ext)) return "video";
  if (AUDIO_EXTS.has(ext)) return "audio";
  if (MARKDOWN_EXTS.has(ext)) return "markdown";
  if (ext === ".txt" || ext === ".log" || ext === ".csv" || ext === "") return "text";
  return "code";
}

/** Files we render as a binary asset — no need to fetch the text body. */
function isBinaryKind(kind: RenderKind): boolean {
  return kind === "image" || kind === "pdf" || kind === "video" || kind === "audio";
}

function langForFenced(filename: string): string {
  const map: Record<string, string> = {
    ".py": "python",
    ".js": "javascript",
    ".ts": "typescript",
    ".tsx": "tsx",
    ".jsx": "jsx",
    ".rs": "rust",
    ".go": "go",
    ".java": "java",
    ".rb": "ruby",
    ".sh": "bash",
    ".bash": "bash",
    ".zsh": "bash",
    ".json": "json",
    ".yaml": "yaml",
    ".yml": "yaml",
    ".toml": "toml",
    ".html": "html",
    ".css": "css",
    ".sql": "sql",
    ".xml": "xml",
    ".c": "c",
    ".cpp": "cpp",
    ".h": "c",
    ".swift": "swift",
    ".kt": "kotlin",
  };
  return map[fileExt(filename)] ?? "";
}

interface TasklistOutputPreviewProps {
  scope: TasklistScope | null;
  tasklistId: string | null;
  filename: string | null;
  onClose: () => void;
}

/**
 * Reusable inline overlay that renders a tasklist output (image / pdf /
 * markdown / code / html / video / audio / text) with copy and download
 * controls. Fills its containing positioned ancestor (`absolute inset-0`).
 *
 * Used by both:
 *  - `TasklistOutputPortal`, which feeds it from `useTasklistOutputStore`
 *    (driven from the InlineTasklistPanel).
 *  - `TaskDetailModal`, which drives it from local state when the user
 *    clicks an output tile inside the task detail view.
 */
export function TasklistOutputPreview({
  scope,
  tasklistId,
  filename,
  onClose,
}: TasklistOutputPreviewProps) {
  const [content, setContent] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [downloading, setDownloading] = useState(false);

  const kind: RenderKind | null = filename ? classifyFile(filename) : null;
  const assetUrl = useMemo(
    () =>
      scope && tasklistId && filename
        ? api.getTasklistOutputUrlForScope(scope, tasklistId, filename)
        : null,
    [scope, tasklistId, filename],
  );

  // Fetch text content only for non-binary file kinds.
  useEffect(() => {
    if (!scope || !tasklistId || !filename || !kind) {
      setContent(null);
      setError(null);
      setLoading(false);
      return;
    }
    if (isBinaryKind(kind)) {
      setContent(null);
      setError(null);
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError(null);
    setContent(null);
    api
      .getTasklistOutputForScope(scope, tasklistId, filename)
      .then((text) => {
        if (cancelled) return;
        setContent(text);
        setLoading(false);
      })
      .catch((err) => {
        if (cancelled) return;
        setError((err as Error).message);
        setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [scope, tasklistId, filename, kind]);

  // Reset transient UI state when the file changes.
  useEffect(() => {
    setCopied(false);
    setDownloading(false);
  }, [filename]);

  const isOpen = scope !== null && tasklistId !== null && filename !== null;

  const handleCopy = async () => {
    if (content === null) return;
    try {
      await navigator.clipboard.writeText(content);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      // Silent — the button just won't flip to "copied".
    }
  };

  const handleDownload = async () => {
    if (!assetUrl || !filename) return;
    setDownloading(true);
    try {
      const baseName = filename.split("/").pop() || filename;
      const dot = baseName.lastIndexOf(".");
      const ext = dot >= 0 ? baseName.slice(dot + 1) : "";
      const savePath = await save({
        defaultPath: baseName,
        filters: ext ? [{ name: ext.toUpperCase(), extensions: [ext] }] : undefined,
      });
      if (!savePath) return; // user cancelled
      const res = await fetch(assetUrl);
      if (!res.ok) throw new Error(`API ${res.status}`);
      const buf = await res.arrayBuffer();
      await writeFile(savePath, new Uint8Array(buf));
    } catch {
      // Silent failure; the button resets.
    } finally {
      setDownloading(false);
    }
  };

  const renderContent = () => {
    if (!filename || !kind) return null;

    if (kind === "image" && assetUrl) {
      return (
        <div className="flex-1 min-h-0 flex items-center justify-center bg-[var(--bg-tertiary)] rounded-[10px] overflow-hidden">
          <img
            src={assetUrl}
            alt={filename}
            className="max-w-full max-h-full object-contain"
          />
        </div>
      );
    }

    if (kind === "pdf" && assetUrl) {
      return (
        <iframe
          src={assetUrl}
          title={filename}
          className="flex-1 min-h-0 w-full rounded-[10px] border-0 bg-white"
        />
      );
    }

    if (kind === "video" && assetUrl) {
      return (
        <div className="flex-1 min-h-0 flex items-center justify-center bg-black rounded-[10px] overflow-hidden">
          <video src={assetUrl} controls className="max-w-full max-h-full" />
        </div>
      );
    }

    if (kind === "audio" && assetUrl) {
      return (
        <div className="flex-1 min-h-0 flex items-center justify-center">
          <audio src={assetUrl} controls className="w-full" />
        </div>
      );
    }

    if (content === null) return null;

    if (kind === "html") {
      return (
        <iframe
          srcDoc={content}
          title={filename}
          sandbox="allow-same-origin"
          className="flex-1 min-h-0 w-full rounded-[10px] border-0 bg-white"
        />
      );
    }

    if (kind === "markdown") {
      return (
        <div className="prose prose-sm dark:prose-invert max-w-none [&_p]:my-2 [&_ul]:my-2 [&_ol]:my-2 [&_li]:my-0.5 [&_h1]:text-[18px] [&_h2]:text-[16px] [&_h3]:text-[14px] [&_pre]:my-3 [&_code]:text-[13px]">
          <Suspense fallback={<Markdown remarkPlugins={[remarkGfm]}>{content}</Markdown>}>
            <LazyRichMarkdown>{content}</LazyRichMarkdown>
          </Suspense>
        </div>
      );
    }

    if (kind === "text") {
      return (
        <pre className="text-[12.5px] text-[var(--text-primary)] whitespace-pre-wrap break-words font-mono leading-relaxed">
          {content}
        </pre>
      );
    }

    // code
    const lang = langForFenced(filename);
    const wrapped = `\`\`\`${lang}\n${content}\n\`\`\``;
    return (
      <div className="prose prose-sm dark:prose-invert max-w-none [&_pre]:my-0 [&_code]:text-[13px]">
        <Markdown remarkPlugins={[remarkGfm]}>{wrapped}</Markdown>
      </div>
    );
  };

  // Asset kinds render their own viewport; text kinds get the scrollable wrapper.
  const isViewportKind = kind === "image" || kind === "pdf" || kind === "video" || kind === "html";
  const canCopy = kind !== null && !isBinaryKind(kind) && content !== null;

  return (
    <AnimatePresence>
      {isOpen && (
        <motion.div
          key={`${tasklistId}:${filename}`}
          className="absolute inset-0 z-30 flex flex-col rounded-[16px] overflow-hidden"
          style={{
            backgroundColor: "var(--bg-secondary)",
          }}
          initial={{ opacity: 0, scale: 0.98 }}
          animate={{ opacity: 1, scale: 1 }}
          exit={{ opacity: 0, scale: 0.98 }}
          transition={{
            scale: { type: "spring", stiffness: 320, damping: 30, mass: 0.7 },
            opacity: { duration: 0.18, ease: "easeOut" },
          }}
        >
          {/* Header */}
          <div
            className="px-4 py-3 flex items-center gap-2 shrink-0 border-b"
            style={{ borderColor: "var(--border-primary)" }}
          >
            <FileText size={14} style={{ color: "var(--text-secondary)" }} />
            <Tooltip placement="top" label={filename ?? ""} className="flex-1 min-w-0">
              <span
                className="block text-[13px] font-semibold truncate font-mono"
                style={{ color: "var(--text-primary)" }}
              >
                {filename ? displayOutputFilename(filename) : ""}
              </span>
            </Tooltip>

            {canCopy && (
              <Tooltip placement="top" label={copied ? "Copied" : "Copy to clipboard"}>
                <button
                  type="button"
                  onClick={handleCopy}
                  aria-label="Copy to clipboard"
                  className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)]"
                  style={{ color: copied ? "#16a34a" : "var(--text-secondary)" }}
                >
                  {copied ? <Check size={14} /> : <Copy size={13} />}
                </button>
              </Tooltip>
            )}

            <Tooltip placement="top" label="Download">
              <button
                type="button"
                onClick={handleDownload}
                disabled={downloading || !assetUrl}
                aria-label="Download"
                className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)] disabled:opacity-50"
                style={{ color: "var(--text-secondary)" }}
              >
                {downloading ? (
                  <Loader2 size={13} className="animate-spin" />
                ) : (
                  <Download size={13} />
                )}
              </button>
            </Tooltip>

            <button
              type="button"
              onClick={onClose}
              aria-label="Close output"
              className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)]"
              style={{ color: "var(--text-secondary)" }}
            >
              <X size={14} />
            </button>
          </div>

          {/* Body */}
          {isViewportKind ? (
            <div className="flex-1 min-h-0 flex flex-col p-3">
              {loading && (
                <div className="flex-1 min-h-0 flex items-center justify-center">
                  <Loader2
                    size={18}
                    className="animate-spin"
                    style={{ color: "var(--text-secondary)" }}
                  />
                </div>
              )}
              {error && (
                <div
                  className="px-3 py-2 rounded-[10px] text-[12px]"
                  style={{ backgroundColor: "rgba(244,63,94,0.12)", color: "#be123c" }}
                >
                  {error}
                </div>
              )}
              {!error && (isBinaryKind(kind ?? "text") || content !== null) && renderContent()}
            </div>
          ) : (
            <div className="flex-1 overflow-y-auto px-4 py-3 min-h-0 custom-scrollbar">
              {loading && (
                <div className="flex items-center justify-center py-12">
                  <Loader2
                    size={18}
                    className="animate-spin"
                    style={{ color: "var(--text-secondary)" }}
                  />
                </div>
              )}
              {error && (
                <div
                  className="px-3 py-2 rounded-[10px] text-[12px]"
                  style={{ backgroundColor: "rgba(244,63,94,0.12)", color: "#be123c" }}
                >
                  {error}
                </div>
              )}
              {!loading && !error && content !== null && renderContent()}
            </div>
          )}
        </motion.div>
      )}
    </AnimatePresence>
  );
}

/**
 * Inline overlay that fills its containing positioned ancestor
 * (mounted inside InlineTasklistPanel). Reads the active output
 * from `useTasklistOutputStore` and delegates rendering to
 * `TasklistOutputPreview`.
 */
export function TasklistOutputPortal() {
  const scope = useTasklistOutputStore((s) => s.scope);
  const tasklistId = useTasklistOutputStore((s) => s.tasklistId);
  const filename = useTasklistOutputStore((s) => s.filename);
  const close = useTasklistOutputStore((s) => s.close);

  return (
    <TasklistOutputPreview
      scope={scope}
      tasklistId={tasklistId}
      filename={filename}
      onClose={close}
    />
  );
}
