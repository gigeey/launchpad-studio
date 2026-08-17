import { useState, useEffect, useRef, useCallback } from "react";

const BASE_URL = import.meta.env.VITE_API_BASE_URL ?? "http://localhost:3001";

interface LogEntry {
  timestamp: string;
  level: string;
  target: string;
  message: string;
}

const levelColor: Record<string, string> = {
  ERROR: "text-red-400",
  WARN: "text-yellow-400",
  INFO: "text-green-400",
  DEBUG: "text-blue-400",
  TRACE: "text-gray-500",
};

type Tab = "all" | "normalizer";

const tabConfig: Record<Tab, { label: string; targetFilter?: string }> = {
  all: { label: "All Logs" },
  normalizer: { label: "Normalizer", targetFilter: "ao_normalizer" },
};

export function DevPanel() {
  const [visible, setVisible] = useState(false);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [autoScroll, setAutoScroll] = useState(true);
  const [filter, setFilter] = useState("");
  const [levelFilter, setLevelFilter] = useState<string>("ALL");
  const [activeTab, setActiveTab] = useState<Tab>("all");
  const logsEndRef = useRef<HTMLDivElement>(null);
  const intervalRef = useRef<ReturnType<typeof setInterval> | undefined>(undefined);

  const fetchLogs = useCallback(async () => {
    try {
      const res = await fetch(`${BASE_URL}/system/logs?limit=500`);
      if (res.ok) {
        const data = await res.json();
        setLogs(data.entries ?? []);
      }
    } catch {
      // silently fail
    }
  }, []);

  useEffect(() => {
    const handler = () => setVisible((v) => !v);
    window.addEventListener("toggle-dev-panel", handler);
    return () => window.removeEventListener("toggle-dev-panel", handler);
  }, []);

  useEffect(() => {
    if (visible) {
      fetchLogs();
      intervalRef.current = setInterval(fetchLogs, 2000);
    } else if (intervalRef.current) {
      clearInterval(intervalRef.current);
    }
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [visible, fetchLogs]);

  useEffect(() => {
    if (autoScroll && logsEndRef.current) {
      logsEndRef.current.scrollIntoView({ behavior: "smooth" });
    }
  }, [logs, autoScroll]);

  const clearLogs = async () => {
    await fetch(`${BASE_URL}/system/logs/clear`, { method: "POST" }).catch(() => {});
    setLogs([]);
  };

  if (!visible) return null;

  const filtered = logs.filter((entry) => {
    const tab = tabConfig[activeTab];
    if (tab.targetFilter && entry.target !== tab.targetFilter) return false;
    if (levelFilter !== "ALL" && entry.level !== levelFilter) return false;
    if (filter && !entry.message.toLowerCase().includes(filter.toLowerCase()) && !entry.target.toLowerCase().includes(filter.toLowerCase())) return false;
    return true;
  });

  return (
    <div className="fixed inset-0 z-[9999] flex flex-col bg-black/95 text-white font-mono text-xs">
      {/* Toolbar */}
      <div className="flex items-center gap-2 pl-20 pr-3 py-2 bg-gray-900 border-b border-gray-700 shrink-0" data-tauri-drag-region>
        <span className="text-green-400 font-bold text-sm">Dev Panel</span>
        <span className="text-gray-500">|</span>

        {(Object.entries(tabConfig) as [Tab, typeof tabConfig[Tab]][]).map(([key, cfg]) => (
          <button
            key={key}
            onClick={() => setActiveTab(key)}
            className={`px-2 py-1 rounded text-xs ${
              activeTab === key
                ? "bg-blue-600 text-white"
                : "bg-gray-800 text-gray-400 hover:bg-gray-700"
            }`}
          >
            {cfg.label}
          </button>
        ))}

        <span className="text-gray-500">|</span>

        <input
          type="text"
          placeholder="Filter logs..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="bg-gray-800 border border-gray-600 rounded px-2 py-1 text-xs w-48 focus:outline-none focus:border-blue-500"
        />

        <select
          value={levelFilter}
          onChange={(e) => setLevelFilter(e.target.value)}
          className="bg-gray-800 border border-gray-600 rounded px-2 py-1 text-xs focus:outline-none"
        >
          <option value="ALL">All Levels</option>
          <option value="ERROR">Error</option>
          <option value="WARN">Warn</option>
          <option value="INFO">Info</option>
          <option value="DEBUG">Debug</option>
          <option value="TRACE">Trace</option>
        </select>

        <label className="flex items-center gap-1 text-gray-400 cursor-pointer">
          <input
            type="checkbox"
            checked={autoScroll}
            onChange={(e) => setAutoScroll(e.target.checked)}
            className="accent-green-500"
          />
          Auto-scroll
        </label>

        <div className="flex-1 h-full cursor-grab active:cursor-grabbing" data-tauri-drag-region />

        <span className="text-gray-500">{filtered.length} entries</span>

        <button
          onClick={fetchLogs}
          className="px-2 py-1 bg-gray-700 hover:bg-gray-600 rounded text-gray-300"
        >
          Refresh
        </button>
        <button
          onClick={clearLogs}
          className="px-2 py-1 bg-gray-700 hover:bg-gray-600 rounded text-gray-300"
        >
          Clear
        </button>
        <button
          onClick={() => setVisible(false)}
          className="px-2 py-1 bg-red-900 hover:bg-red-800 rounded text-red-300"
          title="Close"
        >
          Close
        </button>
      </div>

      {/* Log entries */}
      <div className="flex-1 overflow-y-auto p-2">
        {filtered.length === 0 ? (
          <div className="text-gray-500 text-center mt-8">
            {logs.length === 0 ? "No log entries yet. Server logs will appear here." : "No entries match the current filter."}
          </div>
        ) : (
          filtered.map((entry, i) => (
            <div key={i} className="flex gap-2 py-0.5 hover:bg-gray-900/50 leading-tight">
              <span className="text-gray-600 shrink-0 w-[180px]">{entry.timestamp}</span>
              <span className={`shrink-0 w-[44px] font-bold ${levelColor[entry.level] ?? "text-gray-400"}`}>
                {entry.level.padEnd(5)}
              </span>
              <span className="text-purple-400 shrink-0 max-w-[200px] truncate" title={entry.target}>
                {entry.target}
              </span>
              <span className="text-gray-200 break-all">{entry.message}</span>
            </div>
          ))
        )}
        <div ref={logsEndRef} />
      </div>

      {/* Footer */}
      <div className="px-3 py-1 bg-gray-900 border-t border-gray-700 text-gray-500 text-[10px] shrink-0">
        Logs refresh every 2s &middot; Buffer holds last 2000 entries
      </div>
    </div>
  );
}
