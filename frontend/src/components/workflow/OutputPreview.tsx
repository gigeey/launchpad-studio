import { lazy, Suspense, useEffect, useState } from "react";
import { Loader2, X, FileText } from "lucide-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import * as api from "../../lib/api";

const LazyRichMarkdown = lazy(() =>
  import("../shared/RichMarkdown").then((mod) => ({ default: mod.RichMarkdown }))
);

interface OutputPreviewProps {
  taskId: string;
  filename: string;
  onClose: () => void;
}

/** File extensions that should be rendered as markdown directly. */
const MARKDOWN_EXTENSIONS = new Set([".md", ".markdown", ".mdx"]);

/** Map file extension to a language hint for fenced code blocks. */
function getLang(filename: string): string {
  const ext = filename.slice(filename.lastIndexOf(".")).toLowerCase();
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
  return map[ext] ?? "";
}

function isMarkdown(filename: string): boolean {
  const ext = filename.slice(filename.lastIndexOf(".")).toLowerCase();
  return MARKDOWN_EXTENSIONS.has(ext);
}

function isPlainText(filename: string): boolean {
  const ext = filename.slice(filename.lastIndexOf(".")).toLowerCase();
  return ext === ".txt" || ext === ".log" || ext === ".csv";
}

export function OutputPreview({ taskId, filename, onClose }: OutputPreviewProps) {
  const [content, setContent] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setLoading(true);
    setError(null);
    setContent(null);
    api
      .getTaskOutput(taskId, filename)
      .then((text) => {
        setContent(text);
        setLoading(false);
      })
      .catch((err) => {
        setError((err as Error).message);
        setLoading(false);
      });
  }, [taskId, filename]);

  // Determine how to render
  const renderContent = () => {
    if (!content) return null;

    if (isMarkdown(filename)) {
      return (
        <div className="prose prose-sm dark:prose-invert max-w-none [&_p]:my-2 [&_ul]:my-2 [&_ol]:my-2 [&_li]:my-0.5 [&_h1]:text-[18px] [&_h2]:text-[16px] [&_h3]:text-[14px] [&_pre]:my-3 [&_code]:text-[13px]">
          <Suspense fallback={<Markdown remarkPlugins={[remarkGfm]}>{content}</Markdown>}>
            <LazyRichMarkdown>{content}</LazyRichMarkdown>
          </Suspense>
        </div>
      );
    }

    if (isPlainText(filename)) {
      return (
        <pre className="text-[13px] text-[var(--text-primary)] whitespace-pre-wrap break-words font-mono leading-relaxed">
          {content}
        </pre>
      );
    }

    // Code files and everything else — wrap in fenced code block for syntax highlighting
    const lang = getLang(filename);
    const wrapped = `\`\`\`${lang}\n${content}\n\`\`\``;
    return (
      <div className="prose prose-sm dark:prose-invert max-w-none [&_pre]:my-0 [&_code]:text-[13px]">
        <Markdown remarkPlugins={[remarkGfm]}>{wrapped}</Markdown>
      </div>
    );
  };

  return (
    <div className="flex flex-col flex-1 min-h-0 overflow-hidden">
      {/* Header */}
      <div className="px-6 py-3 border-b border-[var(--border-secondary)] flex items-center justify-between">
        <div className="flex items-center gap-2 min-w-0">
          <FileText size={16} className="flex-shrink-0 text-[var(--text-secondary)]" />
          <span className="text-[13px] font-bold text-[var(--text-primary)] truncate">{filename}</span>
        </div>
        <button
          onClick={onClose}
          className="w-[28px] h-[28px] rounded-[8px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] transition-colors cursor-pointer flex-shrink-0"
        >
          <X size={14} />
        </button>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto px-6 py-4 custom-scrollbar">
        {loading && (
          <div className="flex items-center justify-center py-12">
            <Loader2 size={20} className="animate-spin text-[var(--text-secondary)]" />
          </div>
        )}

        {error && (
          <div className="text-center py-12">
            <p className="text-[13px] text-red-500">Failed to load file: {error}</p>
          </div>
        )}

        {content !== null && !loading && renderContent()}
      </div>
    </div>
  );
}
