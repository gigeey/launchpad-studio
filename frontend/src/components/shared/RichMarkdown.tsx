import { lazy, Suspense } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import rehypeKatex from "rehype-katex";
import "katex/dist/katex.min.css";
import { escapeRawHtmlOutsideCode } from "../../lib/escapeRawHtml";

const LazyMermaidBlock = lazy(() =>
  import("../chat/MermaidBlock").then((mod) => ({ default: mod.MermaidBlock }))
);

/** Rehype plugin: strip stray backtick characters from text nodes. */
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

const remarkPluginsMath = [remarkGfm, remarkMath];
const rehypePluginsMath = [rehypeStripBackticks, rehypeKatex];
const remarkPluginsPlain = [remarkGfm];
const rehypePluginsPlain = [rehypeStripBackticks];

/** Check if content likely contains real LaTeX math (not just currency $). */
function hasLatexMath(text: string): boolean {
  if (text.includes("$$")) return true;
  if (/\$\\[a-zA-Z]/.test(text)) return true;
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
        <Suspense fallback={<div className="my-2 text-xs text-gray-400">Loading diagram...</div>}>
          <LazyMermaidBlock code={String(children).trim()} />
        </Suspense>
      );
    }
    const text = typeof children === 'string' ? children : String(children ?? '');
    const cleaned = text.replace(/`/g, '');
    const hasNewlines = cleaned.includes('\n');

    if (hasNewlines || className) {
      return (
        <code className={`${className ?? ''} block bg-[var(--bg-hover)] rounded-md px-3 py-2 my-2 text-[13px] whitespace-pre-wrap overflow-x-auto`}>
          {cleaned}
        </code>
      );
    }
    return <code className="bg-[var(--bg-hover)] rounded px-1 py-0.5 text-[13px]">{cleaned}</code>;
  },
  pre({ children }: { children?: React.ReactNode }) {
    return <>{children}</>;
  },
};

/** Renders markdown with Mermaid diagram and KaTeX math support.
 *  KaTeX is only enabled when the content actually contains LaTeX patterns. */
export function RichMarkdown({ children }: { children: string }) {
  const useMath = hasLatexMath(children);
  const sanitized = escapeRawHtmlOutsideCode(children);
  return (
    <Markdown
      remarkPlugins={useMath ? remarkPluginsMath : remarkPluginsPlain}
      rehypePlugins={useMath ? rehypePluginsMath : rehypePluginsPlain}
      components={mdComponents}
    >
      {sanitized}
    </Markdown>
  );
}
