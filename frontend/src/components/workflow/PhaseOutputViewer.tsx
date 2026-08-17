import { useCallback, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Eye } from "lucide-react";
import { useWorkflowStore } from "../../stores/workflowStore";
import type { PhaseOutput } from "../../types/workflow";

const TRUNCATE_LENGTH = 2000;

/** Detect content type from filename extension. */
function contentType(filename: string): "json" | "markdown" | "text" {
  if (filename.endsWith(".json")) return "json";
  if (filename.endsWith(".md")) return "markdown";
  return "text";
}

function tryFormatJson(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

/** Render output content with appropriate formatting. */
export function OutputContent({ content, filename }: { content: string; filename: string }) {
  const [showFull, setShowFull] = useState(false);
  const isTruncated = content.length > TRUNCATE_LENGTH;
  const displayed = !showFull && isTruncated ? content.slice(0, TRUNCATE_LENGTH) : content;
  const type = contentType(filename);

  return (
    <div className="mt-[6px]">
      {type === "json" && (
        <pre className="bg-[var(--bg-hover)] rounded-md px-3 py-2 text-[12px] whitespace-pre-wrap overflow-x-auto max-h-[400px] overflow-y-auto custom-scrollbar">
          {tryFormatJson(displayed)}
        </pre>
      )}
      {type === "markdown" && (
        <div className="prose prose-sm max-w-none text-[12px] text-[var(--text-primary)] [&_p]:my-1 [&_h1]:text-[14px] [&_h2]:text-[13px] [&_h3]:text-[12px] [&_code]:bg-[var(--bg-hover)] [&_code]:rounded [&_code]:px-1 [&_code]:py-0.5 [&_code]:text-[12px] [&_pre_code]:block [&_pre_code]:p-2 [&_pre_code]:my-1">
          <Markdown remarkPlugins={[remarkGfm]}>{displayed}</Markdown>
        </div>
      )}
      {type === "text" && (
        <pre className="bg-[var(--bg-hover)] rounded-md px-3 py-2 text-[12px] whitespace-pre-wrap overflow-x-auto max-h-[400px] overflow-y-auto custom-scrollbar font-mono">
          {displayed}
        </pre>
      )}
      {isTruncated && (
        <button
          onClick={() => setShowFull((v) => !v)}
          className="text-[11px] text-[var(--accent)] mt-[4px] cursor-pointer hover:underline"
        >
          {showFull ? "Show less" : "Show full"}
        </button>
      )}
    </div>
  );
}

/** Expandable output viewer for a completed phase's outputs. */
export function PhaseOutputViewer({
  taskId,
  outputs,
}: {
  taskId: string;
  outputs: PhaseOutput[];
}) {
  const fetchOutput = useWorkflowStore((s) => s.fetchOutput);
  const [expanded, setExpanded] = useState(false);
  const [contents, setContents] = useState<Record<string, string>>({});
  const [loading, setLoading] = useState(false);

  const handleExpand = useCallback(async () => {
    if (expanded) {
      setExpanded(false);
      return;
    }
    setExpanded(true);
    const toFetch = outputs.filter((o) => o.filename && !(o.filename in contents));
    if (toFetch.length === 0) return;
    setLoading(true);
    const results: Record<string, string> = { ...contents };
    await Promise.all(
      toFetch.map(async (o) => {
        if (!o.filename) return;
        try {
          results[o.filename] = await fetchOutput(taskId, o.filename);
        } catch {
          results[o.filename] = "(Failed to load output)";
        }
      }),
    );
    setContents(results);
    setLoading(false);
  }, [expanded, outputs, contents, taskId, fetchOutput]);

  if (outputs.length === 0 || outputs.every((o) => !o.filename)) return null;

  return (
    <div className="mt-[4px]">
      <button
        onClick={handleExpand}
        className="flex items-center gap-[4px] text-[11px] text-[var(--accent)] cursor-pointer hover:underline"
      >
        <Eye className="w-[12px] h-[12px]" />
        {expanded ? "Hide Output" : "View Output"}
      </button>
      {expanded && (
        <div className="mt-[4px]">
          {loading && (
            <div className="text-[11px] text-[var(--text-tertiary)]">Loading...</div>
          )}
          {outputs.map((o) => {
            if (!o.filename) return null;
            const content = contents[o.filename];
            if (!content) return null;
            return (
              <div key={o.id} className="mb-[6px]">
                <div className="text-[11px] font-medium text-[var(--text-secondary)]">
                  {o.filename}
                  {o.description && (
                    <span className="font-normal text-[var(--text-tertiary)]">
                      {" "}&mdash; {o.description}
                    </span>
                  )}
                </div>
                <OutputContent content={content} filename={o.filename} />
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
