import { memo, useCallback, useEffect, useRef, useState } from "react";
import { Maximize2 } from "lucide-react";
import { useIsDark } from "../../stores/userPreferencesStore";
import { useMediaPreviewStore } from "../../stores/mediaPreviewStore";

let mermaidInstance: typeof import("mermaid").default | null = null;
let mermaidLoading: Promise<typeof import("mermaid").default> | null = null;
let currentMermaidTheme: string | null = null;

function getResolvedTheme(): "neutral" | "dark" {
  return document.documentElement.getAttribute("data-theme") === "dark" ? "dark" : "neutral";
}

function getMermaid(): Promise<typeof import("mermaid").default> {
  const theme = getResolvedTheme();
  if (mermaidInstance && currentMermaidTheme === theme) return Promise.resolve(mermaidInstance);
  if (mermaidInstance) {
    mermaidInstance.initialize({
      startOnLoad: false,
      theme,
      securityLevel: "strict",
    });
    currentMermaidTheme = theme;
    return Promise.resolve(mermaidInstance);
  }
  if (!mermaidLoading) {
    mermaidLoading = import("mermaid").then((mod) => {
      mermaidInstance = mod.default;
      currentMermaidTheme = theme;
      mermaidInstance.initialize({
        startOnLoad: false,
        theme,
        securityLevel: "strict",
      });
      return mermaidInstance;
    });
  }
  return mermaidLoading;
}

let idCounter = 0;

/** Remove any stray mermaid-generated error elements from document.body. */
function cleanupMermaidErrors() {
  // Mermaid renders temporary containers with ids like "dmermaid-0" directly on <body>.
  // On error these can linger. Also look for mermaid's error icon pattern.
  document.querySelectorAll('body > [id^="dmermaid-"], body > svg[id^="mermaid-"]').forEach((el) => {
    el.remove();
  });
  // Mermaid may also render a full-width error block at the body level
  document.querySelectorAll('body > #mermaid-error, body > .mermaid').forEach((el) => {
    el.remove();
  });
}

const svgCache = new Map<string, { svg: string; height: number }>();

function cacheKey(code: string): string {
  return `${getResolvedTheme()}::${code}`;
}

export const MermaidBlock = memo(function MermaidBlock({ code }: { code: string }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const sentinelRef = useRef<HTMLDivElement>(null);
  const isDark = useIsDark();
  const openPreview = useMediaPreviewStore((s) => s.openPreview);
  const theme = isDark ? "dark" : "neutral";
  const cached = svgCache.get(cacheKey(code));
  const [svg, setSvg] = useState<string | null>(cached?.svg ?? null);
  const [error, setError] = useState<string | null>(null);
  const [isInView, setIsInView] = useState(false);
  const [rendered, setRendered] = useState(!!cached);

  // IntersectionObserver to detect when block enters viewport
  const observerRef = useRef<IntersectionObserver | null>(null);
  const sentinelCallback = useCallback((node: HTMLDivElement | null) => {
    if (observerRef.current) {
      observerRef.current.disconnect();
      observerRef.current = null;
    }
    if (node) {
      observerRef.current = new IntersectionObserver(
        ([entry]) => {
          if (entry.isIntersecting) {
            setIsInView(true);
            // Once in view, stop observing
            observerRef.current?.disconnect();
          }
        },
        { rootMargin: "100px" },
      );
      observerRef.current.observe(node);
    }
    sentinelRef.current = node;
  }, []);

  // Cleanup observer on unmount
  useEffect(() => {
    return () => {
      observerRef.current?.disconnect();
    };
  }, []);

  // Re-render from cache when theme changes
  useEffect(() => {
    const cachedEntry = svgCache.get(cacheKey(code));
    if (cachedEntry) {
      setSvg(cachedEntry.svg);
      setRendered(true);
    } else if (rendered) {
      // Theme changed but no cache for new theme — need to re-render
      setSvg(null);
      setRendered(false);
      setIsInView(true); // force re-render trigger
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [theme]);

  // Trigger mermaid.render() only when in view and not cached
  useEffect(() => {
    if (svgCache.has(cacheKey(code))) return;
    if (!isInView) return;

    const id = `mermaid-${idCounter++}`;
    let cancelled = false;

    getMermaid()
      .then(async (m) => {
        // Validate syntax first to avoid mermaid injecting error SVGs into the DOM
        await m.parse(code);
        return m.render(id, code);
      })
      .then(({ svg }) => {
        if (!cancelled) {
          setSvg(svg);
          setRendered(true);
        }
      })
      .catch((err) => {
        if (!cancelled) setError(String(err));
        // Clean up stray mermaid error elements injected into document body.
        // Mermaid creates temporary containers with id="d{id}" or inserts error SVGs directly.
        cleanupMermaidErrors();
      });

    return () => {
      cancelled = true;
    };
  }, [code, isInView, theme]);

  // Cache successful renders with measured height
  useEffect(() => {
    if (svg && !svgCache.has(cacheKey(code)) && containerRef.current) {
      const height = containerRef.current.getBoundingClientRect().height;
      svgCache.set(cacheKey(code), { svg, height });
    }
  }, [svg, code]);

  if (error) {
    // Extract a concise error message (mermaid errors can be verbose with stack traces)
    const shortError = error.replace(/^Error:\s*/i, '').split('\n')[0].slice(0, 200);
    return (
      <div className="my-2 p-3 rounded-lg border border-[var(--border-secondary)] bg-[var(--bg-tertiary)] text-xs overflow-hidden">
        <div className="flex items-center gap-1.5 text-[var(--text-secondary)] mb-1">
          <span>⚠️</span>
          <span className="font-medium">Diagram syntax error</span>
        </div>
        <pre className="text-[var(--text-tertiary)] whitespace-pre-wrap break-words">{shortError}</pre>
      </div>
    );
  }

  // Cached SVGs render immediately regardless of viewport
  if (cached || rendered) {
    if (svg) {
      return (
        <div
          className="group relative my-2 cursor-pointer overflow-x-auto rounded-lg transition-shadow hover:ring-2 hover:ring-[var(--accent)]/30"
          onClick={() => openPreview({ content: svg, contentType: 'svg', filename: 'diagram.svg' })}
        >
          <div
            ref={containerRef}
            dangerouslySetInnerHTML={{ __html: svg }}
          />
          <div className="absolute top-2 right-2 flex items-center gap-1 rounded-md bg-black/50 px-1.5 py-1 text-white opacity-0 transition-opacity group-hover:opacity-100">
            <Maximize2 size={14} />
          </div>
        </div>
      );
    }
  }

  // Not cached and not in view (or in view but still loading): show placeholder or spinner
  if (!isInView) {
    const cachedHeight = svgCache.get(cacheKey(code))?.height;
    return (
      <div
        ref={sentinelCallback}
        className="my-2 bg-[var(--bg-tertiary)] rounded-lg border border-[var(--border-secondary)]"
        style={cachedHeight ? { height: cachedHeight } : { minHeight: 200 }}
      />
    );
  }

  // In view but still loading (mermaid.render in progress)
  return (
    <div ref={sentinelCallback} className="my-2 flex items-center gap-2 px-3 py-4 bg-[var(--bg-tertiary)] rounded-lg border border-[var(--border-secondary)] text-sm text-[var(--text-tertiary)] animate-pulse">
      <svg className="w-4 h-4 animate-spin" viewBox="0 0 24 24" fill="none">
        <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
        <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
      </svg>
      Rendering diagram…
    </div>
  );
});
