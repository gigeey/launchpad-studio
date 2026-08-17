import { useRef, useEffect, useCallback } from "react";
import { useNavigate } from "react-router-dom";
import { Search, Loader2, X } from "lucide-react";
import { useSearchStore, type SearchResult } from "../../stores/searchStore";

function formatTimestamp(ts: string): string {
  const date = new Date(ts);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));

  if (diffDays === 0) {
    return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  if (diffDays === 1) return "Yesterday";
  if (diffDays < 7) return `${diffDays}d ago`;
  return date.toLocaleDateString([], { month: "short", day: "numeric" });
}

function truncateContent(content: string, maxLen = 120): string {
  if (content.length <= maxLen) return content;
  return content.slice(0, maxLen).trimEnd() + "\u2026";
}

function ResultRow({ result, isSelected, onClick }: { result: SearchResult; isSelected: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      className={`w-full text-left px-3 py-2 cursor-pointer transition-colors duration-100 ${
        isSelected ? "bg-[var(--modal-bg-hover)]" : "hover:bg-[var(--modal-bg-hover)]"
      }`}
      onClick={onClick}
    >
      <div className="flex items-center justify-between gap-2 mb-0.5">
        <span className="text-[12px] font-medium text-[var(--modal-text-primary)] truncate">
          {result.agent_name}
        </span>
        <span className="text-[11px] text-[var(--modal-text-tertiary)] whitespace-nowrap flex-shrink-0">
          {formatTimestamp(result.entry.ts)}
        </span>
      </div>
      <div className="text-[12px] text-[var(--modal-text-secondary)] leading-[1.4] line-clamp-2">
        {truncateContent(result.entry.content)}
      </div>
    </button>
  );
}

export function SearchBar() {
  const inputRef = useRef<HTMLInputElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const navigate = useNavigate();

  const {
    query,
    results,
    isSearching,
    selectedIndex,
    isOpen,
    setQuery,
    clearSearch,
    setSelectedIndex,
    openSearch,
    closeSearch,
    setNavigateTarget,
  } = useSearchStore();

  // Close dropdown on outside click
  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        closeSearch();
      }
    }
    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, [closeSearch]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Escape") {
        closeSearch();
        inputRef.current?.blur();
        return;
      }

      if (!isOpen || results.length === 0) return;

      if (e.key === "Enter") {
        e.preventDefault();
        if (selectedIndex !== null && results[selectedIndex]) {
          handleResultClick(results[selectedIndex], selectedIndex);
        }
        return;
      }

      if (e.key === "ArrowDown") {
        e.preventDefault();
        const next = selectedIndex === null ? 0 : Math.min(selectedIndex + 1, results.length - 1);
        setSelectedIndex(next);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        const prev = selectedIndex === null ? results.length - 1 : Math.max(selectedIndex - 1, 0);
        setSelectedIndex(prev);
      }
    },
    [isOpen, results, selectedIndex, setSelectedIndex, closeSearch, setNavigateTarget, navigate]
  );

  const handleFocus = () => {
    openSearch();
  };

  const handleChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    setQuery(e.target.value);
    if (!isOpen) openSearch();
  };

  const handleClear = () => {
    clearSearch();
    inputRef.current?.focus();
  };

  const handleResultClick = (result: SearchResult, _index: number) => {
    setNavigateTarget({
      agentId: result.agent_id,
      ts: result.entry.ts,
      content: result.entry.content,
      role: result.entry.role,
      agentName: result.agent_name,
    });
    closeSearch();
    navigate(`/chat/${result.agent_id}`);
  };

  const showDropdown = isOpen && (query.trim().length > 0);

  return (
    <div ref={containerRef} className="relative">
      {/* Search input */}
      <div className="app-search-surface border-[1px] border-[var(--search-border)] h-[30px] min-w-[320px] flex items-center gap-1 px-[10px] rounded-[8px] bg-[var(--search-bg)] text-[var(--text-secondary)]">
        {isSearching ? (
          <Loader2 className="w-[16px] h-[16px] text-[var(--text-secondary)] animate-spin flex-shrink-0" />
        ) : (
          <Search className="w-[16px] h-[16px] text-[var(--text-secondary)] flex-shrink-0" />
        )}
        <input
          ref={inputRef}
          type="text"
          value={query}
          onChange={handleChange}
          onFocus={handleFocus}
          onKeyDown={handleKeyDown}
          placeholder="Search"
          className="flex-1 bg-transparent text-[13px] text-[var(--text-primary)] placeholder:text-[var(--text-secondary)] outline-none border-none"
        />
        {query && (
          <button
            type="button"
            onClick={handleClear}
            className="flex-shrink-0 hover:text-[var(--text-primary)] transition-colors cursor-pointer"
          >
            <X className="w-[14px] h-[14px]" />
          </button>
        )}
      </div>

      {/* Dropdown */}
      {showDropdown && (
        <div className="app-search-results absolute top-[calc(100%+4px)] right-0 w-[400px] max-h-[400px] overflow-y-auto rounded-[8px] bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)] shadow-lg z-50">
          {isSearching && results.length === 0 ? (
            <div className="flex items-center justify-center py-6 text-[13px] text-[var(--modal-text-secondary)]">
              <Loader2 className="w-4 h-4 animate-spin mr-2" />
              Searching...
            </div>
          ) : results.length === 0 ? (
            <div className="py-6 text-center text-[13px] text-[var(--modal-text-secondary)]">
              No results found
            </div>
          ) : (
            <div className="py-1">
              {results.map((result, i) => (
                <ResultRow
                  key={`${result.agent_id}-${result.entry.ts}`}
                  result={result}
                  isSelected={selectedIndex === i}
                  onClick={() => handleResultClick(result, i)}
                />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
