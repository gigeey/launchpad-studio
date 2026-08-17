import { useCallback, useEffect, useMemo, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { createPortal } from "react-dom";
import {
  X,
  Puzzle,
  Layers,
  AlertTriangle,
  RefreshCw,
  Trash2,
  Github,
  Folder,
  Plus,
  Loader2,
  FolderOpen,
  ChevronDown,
  ChevronRight,
  Search,
} from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { readTextFile } from "@tauri-apps/plugin-fs";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import { useCollectionsModalStore } from "../stores/collectionsModalStore";
import { MarkdownViewerModal } from "./MarkdownViewerModal";
import ConfirmDialog from "./ui/ConfirmDialog";
import { ConnectorsTab, AddConnectorDialog } from "./ConnectorsTab";
import { listMcpServers } from "../lib/api";
import type {
  PluginSkill,
  PluginEntry,
  PluginCommandError,
  RefreshOutcome,
  UninstallOutcome,
  InstallOutcome,
  PluginInstallSource,
} from "../types/plugins";
import type { McpServerStatus } from "../types/connectors";

type CollectionsTabId = "plugins" | "skills" | "connectors";

const TABS: { id: CollectionsTabId; label: string }[] = [
  { id: "plugins", label: "Plugins" },
  { id: "skills", label: "Skills" },
  { id: "connectors", label: "Connectors" },
];

interface ViewerState {
  title: string;
  content: string;
}

type LoadState<T> =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ready"; data: T }
  | { status: "error"; message: string };

interface RowBusy {
  refreshing?: boolean;
  refreshError?: string;
  togglePending?: boolean;
}

function errorMessage(err: unknown): string {
  if (err && typeof err === "object" && "type" in err) {
    const e = err as PluginCommandError;
    if ("detail" in e && typeof e.detail === "string") return e.detail;
    return e.type;
  }
  if (err instanceof Error) return err.message;
  return String(err);
}

function formatRelativeTimestamp(iso: string): string {
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) return iso;
  const diffMs = Date.now() - parsed.getTime();
  const diffMin = Math.round(diffMs / 60_000);
  if (diffMin < 1) return "just now";
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHr = Math.round(diffMin / 60);
  if (diffHr < 24) return `${diffHr}h ago`;
  const diffDay = Math.round(diffHr / 24);
  if (diffDay < 30) return `${diffDay}d ago`;
  return parsed.toLocaleDateString();
}

export function CollectionsModal() {
  const { isOpen, close } = useCollectionsModalStore();
  const [activeTab, setActiveTab] = useState<CollectionsTabId>("plugins");
  const [pluginsState, setPluginsState] = useState<LoadState<PluginEntry[]>>({
    status: "idle",
  });
  const [skillsState, setSkillsState] = useState<LoadState<PluginSkill[]>>({
    status: "idle",
  });
  const [rowBusy, setRowBusy] = useState<Record<string, RowBusy>>({});
  const [deleteTarget, setDeleteTarget] = useState<PluginEntry | null>(null);
  const [viewerItem, setViewerItem] = useState<ViewerState | null>(null);
  const [inlineError, setInlineError] = useState<string | null>(null);
  const [installDialogOpen, setInstallDialogOpen] = useState(false);
  const [connectorsState, setConnectorsState] = useState<LoadState<McpServerStatus[]>>({ status: "idle" });
  const [addConnectorOpen, setAddConnectorOpen] = useState(false);

  useEffect(() => {
    if (!isOpen) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [isOpen, close]);

  useEffect(() => {
    if (isOpen) setActiveTab("plugins");
  }, [isOpen]);

  const loadPlugins = useCallback(async () => {
    setPluginsState({ status: "loading" });
    try {
      const data = await invoke<PluginEntry[]>("list_plugins");
      setPluginsState({ status: "ready", data });
    } catch (err) {
      setPluginsState({ status: "error", message: errorMessage(err) });
    }
  }, []);

  const loadSkills = useCallback(async () => {
    setSkillsState({ status: "loading" });
    try {
      const data = await invoke<PluginSkill[]>("list_global_skills");
      setSkillsState({ status: "ready", data });
    } catch (err) {
      setSkillsState({ status: "error", message: errorMessage(err) });
    }
  }, []);

  const loadConnectors = useCallback(async () => {
    setConnectorsState({ status: "loading" });
    try {
      const data = await listMcpServers();
      setConnectorsState({ status: "ready", data });
    } catch (err) {
      setConnectorsState({ status: "error", message: errorMessage(err) });
    }
  }, []);

  useEffect(() => {
    if (!isOpen) return;
    if (pluginsState.status === "idle") void loadPlugins();
    if (skillsState.status === "idle") void loadSkills();
    if (connectorsState.status === "idle") void loadConnectors();
  }, [
    isOpen,
    pluginsState.status,
    skillsState.status,
    connectorsState.status,
    loadPlugins,
    loadSkills,
    loadConnectors,
  ]);

  useEffect(() => {
    if (!isOpen) {
      setPluginsState({ status: "idle" });
      setSkillsState({ status: "idle" });
      setConnectorsState({ status: "idle" });
      setRowBusy({});
      setDeleteTarget(null);
      setViewerItem(null);
      setInlineError(null);
      setInstallDialogOpen(false);
      setAddConnectorOpen(false);
    }
  }, [isOpen]);

  const handleInstallSucceeded = useCallback(
    (outcome: InstallOutcome) => {
      setInstallDialogOpen(false);
      // Force the Collections tab to pull a fresh list; Skills will
      // refresh the next time the user switches tabs (they reset on open).
      setPluginsState({ status: "idle" });
      setSkillsState({ status: "idle" });
      setInlineError(null);
      setActiveTab("plugins");
      void outcome; // name/version are available if later stories want toasts.
    },
    [],
  );

  const openSkill = async (skill: PluginSkill) => {
    setInlineError(null);
    try {
      const content = await readTextFile(skill.skill_md);
      setViewerItem({ title: skill.id, content });
    } catch (err) {
      setInlineError(`Failed to read ${skill.id}: ${errorMessage(err)}`);
    }
  };

  const handleToggleAutoUpdate = useCallback(
    async (name: string, next: boolean) => {
      setInlineError(null);
      setPluginsState((prev) =>
        prev.status === "ready"
          ? {
            status: "ready",
            data: prev.data.map((p) =>
              p.name === name ? { ...p, auto_update_enabled: next } : p,
            ),
          }
          : prev,
      );
      setRowBusy((r) => ({ ...r, [name]: { ...r[name], togglePending: true } }));
      try {
        const updated = await invoke<PluginEntry>("set_plugin_auto_update", {
          name,
          enabled: next,
        });
        setPluginsState((prev) =>
          prev.status === "ready"
            ? {
              status: "ready",
              data: prev.data.map((p) => (p.name === name ? updated : p)),
            }
            : prev,
        );
      } catch (err) {
        // Revert the optimistic flip.
        setPluginsState((prev) =>
          prev.status === "ready"
            ? {
              status: "ready",
              data: prev.data.map((p) =>
                p.name === name ? { ...p, auto_update_enabled: !next } : p,
              ),
            }
            : prev,
        );
        setInlineError(
          `Couldn't update auto-update for ${name}: ${errorMessage(err)}`,
        );
      } finally {
        setRowBusy((r) => ({ ...r, [name]: { ...r[name], togglePending: false } }));
      }
    },
    [],
  );

  const handleRefresh = useCallback(async (name: string) => {
    setRowBusy((r) => ({
      ...r,
      [name]: { ...r[name], refreshing: true, refreshError: undefined },
    }));
    try {
      const outcome = await invoke<RefreshOutcome>("refresh_plugin", { name });
      setPluginsState((prev) =>
        prev.status === "ready"
          ? {
            status: "ready",
            data: prev.data.map((p) =>
              p.name === name
                ? {
                  ...p,
                  version: outcome.version,
                  manifest_location: outcome.manifest_location,
                  last_updated_at: outcome.last_updated_at,
                }
                : p,
            ),
          }
          : prev,
      );
    } catch (err) {
      setRowBusy((r) => ({
        ...r,
        [name]: { ...r[name], refreshError: errorMessage(err) },
      }));
    } finally {
      setRowBusy((r) => ({ ...r, [name]: { ...r[name], refreshing: false } }));
    }
  }, []);

  const handleDismissRowError = useCallback((name: string) => {
    setRowBusy((r) => ({ ...r, [name]: { ...r[name], refreshError: undefined } }));
  }, []);

  const handleConfirmDelete = useCallback(async () => {
    const target = deleteTarget;
    if (!target) return;
    try {
      await invoke<UninstallOutcome>("uninstall_plugin", { name: target.name });
      setPluginsState((prev) =>
        prev.status === "ready"
          ? {
            status: "ready",
            data: prev.data.filter((p) => p.name !== target.name),
          }
          : prev,
      );
      setDeleteTarget(null);
    } catch (err) {
      setInlineError(`Couldn't delete ${target.name}: ${errorMessage(err)}`);
      setDeleteTarget(null);
    }
  }, [deleteTarget]);

  const collectionsView = useMemo(
    () => (
      <CollectionsTab
        state={pluginsState}
        rowBusy={rowBusy}
        onToggleAutoUpdate={handleToggleAutoUpdate}
        onRefresh={handleRefresh}
        onRequestDelete={(plugin) => setDeleteTarget(plugin)}
        onDismissRowError={handleDismissRowError}
        onRequestInstall={() => setInstallDialogOpen(true)}
      />
    ),
    [
      pluginsState,
      rowBusy,
      handleToggleAutoUpdate,
      handleRefresh,
      handleDismissRowError,
    ],
  );

  return (
    <AnimatePresence>
      {isOpen && (
        <div
          className="fixed inset-0 z-[300] flex items-center justify-center"
          role="dialog"
          aria-modal="true"
          aria-label="Collections"
        >
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
            className="absolute inset-0 bg-black/40"
            onClick={close}
          />

          <motion.div
            initial={{ opacity: 0, scale: 0.96 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.96 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="collections-modal relative w-full max-w-[780px] h-[640px] rounded-[16px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)] flex flex-col"
            style={{
              boxShadow:
                "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)",
            }}
          >
            <div className="flex items-center justify-between px-[24px] pt-[18px] pb-[12px]">
              <h2 className="text-[18px] font-semibold text-[var(--modal-text-primary)]">
                Collections
              </h2>
              <button
                type="button"
                onClick={close}
                aria-label="Close"
                title="Close"
                className="w-[32px] h-[32px] rounded-[8px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
              >
                <X className="w-[18px] h-[18px]" />
              </button>
            </div>

            <div
              role="tablist"
              className="flex items-center gap-[4px] px-[20px] border-b border-[var(--modal-border-secondary)]"
            >
              {TABS.map((t) => {
                const active = activeTab === t.id;
                return (
                  <button
                    key={t.id}
                    type="button"
                    role="tab"
                    aria-selected={active}
                    onClick={() => setActiveTab(t.id)}
                    className={`relative px-[14px] py-[10px] text-[14px] font-medium transition-colors cursor-pointer ${active
                      ? "text-[var(--modal-text-primary)]"
                      : "text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)]"
                      }`}
                  >
                    {t.label}
                    {active && (
                      <span className="absolute left-[10px] right-[10px] bottom-[-1px] h-[2px] rounded-full bg-[var(--modal-accent)]" />
                    )}
                  </button>
                );
              })}
            </div>

            <div className="flex flex-col flex-1 min-h-0 bg-[var(--modal-bg-primary)]">
              {activeTab === "plugins" && collectionsView}

              {activeTab === "skills" && (
                <SkillsTab state={skillsState} onOpen={openSkill} />
              )}

              {activeTab === "connectors" && (
                <ConnectorsTab
                  state={connectorsState}
                  onRefresh={loadConnectors}
                  onRequestAdd={() => setAddConnectorOpen(true)}
                />
              )}
            </div>

            {inlineError && (
              <div className="absolute bottom-[16px] left-[24px] right-[24px] z-[10] flex items-center gap-[8px] rounded-[10px] border border-[var(--error)] bg-[var(--modal-bg)] px-[12px] py-[8px] text-[12px] text-[var(--error)] shadow-lg">
                <AlertTriangle className="w-[14px] h-[14px] flex-shrink-0" />
                <span className="flex-1">{inlineError}</span>
                <button
                  type="button"
                  onClick={() => setInlineError(null)}
                  className="text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)] cursor-pointer"
                  aria-label="Dismiss"
                >
                  <X className="w-[12px] h-[12px]" />
                </button>
              </div>
            )}
          </motion.div>

          <MarkdownViewerModal
            open={viewerItem !== null}
            onClose={() => setViewerItem(null)}
            title={viewerItem?.title ?? ""}
            content={viewerItem?.content ?? ""}
          />

          <ConfirmDialog
            open={deleteTarget !== null}
            title="Delete plugin"
            message={
              deleteTarget ? (
                <>
                  Remove <span className="font-semibold text-[var(--modal-text-primary)]">{deleteTarget.name}</span>{" "}
                  from Collections? This deletes its skills and rules from the
                  global store and cannot be undone.
                </>
              ) : (
                ""
              )
            }
            confirmLabel="Delete"
            destructive
            onConfirm={handleConfirmDelete}
            onCancel={() => setDeleteTarget(null)}
          />

          <InstallPluginDialog
            open={installDialogOpen}
            onCancel={() => setInstallDialogOpen(false)}
            onInstalled={handleInstallSucceeded}
          />

          <AddConnectorDialog
            open={addConnectorOpen}
            onCancel={() => setAddConnectorOpen(false)}
            onAdded={() => {
              setAddConnectorOpen(false);
              void loadConnectors();
            }}
          />
        </div>
      )}
    </AnimatePresence>
  );
}

function CollectionsTab({
  state,
  rowBusy,
  onToggleAutoUpdate,
  onRefresh,
  onRequestDelete,
  onDismissRowError,
  onRequestInstall,
}: {
  state: LoadState<PluginEntry[]>;
  rowBusy: Record<string, RowBusy>;
  onToggleAutoUpdate: (name: string, next: boolean) => void;
  onRefresh: (name: string) => void;
  onRequestDelete: (plugin: PluginEntry) => void;
  onDismissRowError: (name: string) => void;
  onRequestInstall: () => void;
}) {
  const [search, setSearch] = useState("");

  return (
    <div className="flex-1 flex flex-col min-h-0">
      <div className="flex items-center gap-[10px] px-[24px] pt-[16px] pb-[8px]">
        <div className="relative flex-1 max-w-[320px]">
          <Search className="absolute left-[10px] top-1/2 -translate-y-1/2 w-[14px] h-[14px] text-[var(--modal-text-tertiary)] pointer-events-none" />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search Plugin"
            className="w-full h-[34px] pl-[32px] pr-[12px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors"
          />
        </div>
        <div className="flex-1" />
        <button
          type="button"
          onClick={onRequestInstall}
          className="inline-flex items-center gap-[6px] h-[30px] px-[12px] rounded-[8px] text-[13px] font-medium text-white bg-[var(--modal-accent)] hover:opacity-90 transition-opacity cursor-pointer"
        >
          <Plus className="w-[14px] h-[14px]" />
          <span>Install</span>
        </button>
      </div>
      <div className="flex-1 min-h-0">
        <CollectionsTabBody
          state={state}
          rowBusy={rowBusy}
          search={search}
          onToggleAutoUpdate={onToggleAutoUpdate}
          onRefresh={onRefresh}
          onRequestDelete={onRequestDelete}
          onDismissRowError={onDismissRowError}
          onRequestInstall={onRequestInstall}
        />
      </div>
    </div>
  );
}

function CollectionsTabBody({
  state,
  rowBusy,
  search,
  onToggleAutoUpdate,
  onRefresh,
  onRequestDelete,
  onDismissRowError,
  onRequestInstall,
}: {
  state: LoadState<PluginEntry[]>;
  rowBusy: Record<string, RowBusy>;
  search: string;
  onToggleAutoUpdate: (name: string, next: boolean) => void;
  onRefresh: (name: string) => void;
  onRequestDelete: (plugin: PluginEntry) => void;
  onDismissRowError: (name: string) => void;
  onRequestInstall: () => void;
}) {
  if (state.status === "loading" || state.status === "idle") {
    return <LoadingPane />;
  }
  if (state.status === "error") {
    return <ErrorPane icon={Layers} message={state.message} />;
  }
  if (state.data.length === 0) {
    return (
      <EmptyPane
        icon={Layers}
        title="No plugins installed"
        subtitle="Click Install to add one."
        action={{ label: "Install", onClick: onRequestInstall }}
      />
    );
  }
  const q = search.trim().toLowerCase();
  const filtered = q
    ? state.data.filter(
      (p) =>
        p.name.toLowerCase().includes(q) ||
        p.source.value.toLowerCase().includes(q),
    )
    : state.data;
  if (filtered.length === 0) {
    return (
      <div className="flex-1 flex items-center justify-center px-[24px] py-[20px]">
        <p className="text-[13px] text-[var(--modal-text-tertiary)]">
          No plugins match "{search.trim()}". Try a different search term.
        </p>
      </div>
    );
  }
  return (
    <div className="flex-1 overflow-y-auto px-[24px] py-[8px] pb-[16px]">
      <div className="grid grid-cols-2 gap-[14px] auto-rows-min">
        {filtered.map((plugin) => (
          <PluginTile
            key={plugin.name}
            plugin={plugin}
            busy={rowBusy[plugin.name] ?? {}}
            onToggleAutoUpdate={onToggleAutoUpdate}
            onRefresh={onRefresh}
            onRequestDelete={onRequestDelete}
            onDismissRowError={onDismissRowError}
          />
        ))}
      </div>
    </div>
  );
}

function PluginTile({
  plugin,
  busy,
  onToggleAutoUpdate,
  onRefresh,
  onRequestDelete,
  onDismissRowError,
}: {
  plugin: PluginEntry;
  busy: RowBusy;
  onToggleAutoUpdate: (name: string, next: boolean) => void;
  onRefresh: (name: string) => void;
  onRequestDelete: (plugin: PluginEntry) => void;
  onDismissRowError: (name: string) => void;
}) {
  const refreshing = !!busy.refreshing;
  const togglePending = !!busy.togglePending;
  const refreshError = busy.refreshError;
  const stale = !!refreshError;

  const sourceLabel = plugin.source.value;
  const SourceIcon = plugin.source.type === "github_url" ? Github : Folder;

  return (
    <div
      className={`rounded-[14px] border bg-[var(--modal-bg)] px-[16px] py-[14px] flex flex-col gap-[8px] transition-colors ${stale
        ? "border-[var(--error)]"
        : "border-[var(--modal-border-secondary)]"
        }`}
    >
      <div className="flex items-start justify-between gap-[12px]">
        <div className="flex items-baseline gap-[8px] min-w-0 flex-1">
          <h3
            className="text-[15px] font-semibold text-[var(--modal-text-primary)] truncate"
            title={plugin.name}
          >
            {plugin.name}
          </h3>
          <span className="text-[12px] text-[var(--modal-text-tertiary)] flex-shrink-0">
            v{plugin.version}
          </span>
          {stale && (
            <span
              className="text-[11px] font-medium text-[var(--error)] flex-shrink-0"
              title="Last refresh failed"
            >
              stale
            </span>
          )}
        </div>
        <button
          type="button"
          onClick={() => onRefresh(plugin.name)}
          disabled={refreshing}
          aria-label={`Refresh ${plugin.name}`}
          title="Refresh"
          className="h-[28px] w-[28px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:cursor-not-allowed disabled:opacity-60 flex-shrink-0"
        >
          <RefreshCw
            className={`w-[14px] h-[14px] ${refreshing ? "animate-spin" : ""}`}
          />
        </button>
      </div>

      <div
        className="flex items-center gap-[6px] text-[13px] text-[var(--modal-text-secondary)] leading-[18px] min-h-[36px] min-w-0"
        title={sourceLabel}
      >
        <SourceIcon className="w-[12px] h-[12px] flex-shrink-0" />
        <span className="truncate">{sourceLabel}</span>
      </div>

      <div className="border-t border-[var(--modal-border-secondary)] -mx-[16px]" />

      <div className="flex items-center justify-between gap-[8px]">
        <span className="text-[12px] text-[var(--modal-text-tertiary)] truncate">
          Updated {formatRelativeTimestamp(plugin.last_updated_at)}
        </span>
        <div className="flex items-center gap-[8px] flex-shrink-0">
          <label
            className={`inline-flex items-center gap-[6px] text-[12px] text-[var(--modal-text-secondary)] select-none ${togglePending ? "opacity-60 cursor-default" : "cursor-pointer"
              }`}
            title="Auto-refresh"
          >
            <input
              type="checkbox"
              checked={plugin.auto_update_enabled}
              disabled={togglePending}
              onChange={(e) =>
                onToggleAutoUpdate(plugin.name, e.target.checked)
              }
              className="w-[14px] h-[14px] rounded-[3px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg-primary)] accent-[var(--modal-accent)] cursor-pointer disabled:cursor-default"
            />
            <span>Auto-refresh</span>
          </label>
          <button
            type="button"
            onClick={() => onRequestDelete(plugin)}
            aria-label={`Delete ${plugin.name}`}
            title="Delete"
            className="h-[28px] w-[28px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:text-[var(--error)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
          >
            <Trash2 className="w-[14px] h-[14px]" />
          </button>
        </div>
      </div>

      {refreshError && (
        <div className="flex items-start gap-[8px] rounded-[8px] border border-[var(--error)] bg-[var(--modal-bg-primary)] px-[10px] py-[8px] text-[12px] text-[var(--error)]">
          <AlertTriangle className="w-[12px] h-[12px] mt-[2px] flex-shrink-0" />
          <span className="flex-1">Refresh failed: {refreshError}</span>
          <button
            type="button"
            onClick={() => onDismissRowError(plugin.name)}
            aria-label="Dismiss refresh error"
            className="text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)] cursor-pointer"
          >
            <X className="w-[12px] h-[12px]" />
          </button>
        </div>
      )}
    </div>
  );
}

function SkillsTab({
  state,
  onOpen,
}: {
  state: LoadState<PluginSkill[]>;
  onOpen: (skill: PluginSkill) => void;
}) {
  const [expandedPlugins, setExpandedPlugins] = useState<
    Record<string, boolean>
  >({});

  if (state.status === "loading" || state.status === "idle") {
    return <LoadingPane />;
  }
  if (state.status === "error") {
    return <ErrorPane icon={Puzzle} message={state.message} />;
  }
  if (state.data.length === 0) {
    return (
      <EmptyPane
        icon={Puzzle}
        title="No skills installed yet"
        subtitle="Install a plugin from the Collections tab."
      />
    );
  }

  const grouped = new Map<string, PluginSkill[]>();
  for (const skill of state.data) {
    const list = grouped.get(skill.plugin_name);
    if (list) list.push(skill);
    else grouped.set(skill.plugin_name, [skill]);
  }
  const pluginNames = Array.from(grouped.keys()).sort((a, b) =>
    a.localeCompare(b),
  );

  return (
    <div className="flex-1 overflow-y-auto px-[24px] py-[16px]">
      <div className="flex flex-col gap-[10px]">
        {pluginNames.map((name) => {
          const skills = grouped.get(name)!;
          const expanded = expandedPlugins[name] === true;
          return (
            <div
              key={name}
              className="rounded-[14px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg)] overflow-hidden"
            >
              <button
                type="button"
                onClick={() =>
                  setExpandedPlugins((e) => ({ ...e, [name]: !e[name] }))
                }
                aria-expanded={expanded}
                aria-label={expanded ? `Collapse ${name}` : `Expand ${name}`}
                className="w-full flex items-center gap-[12px] px-[16px] py-[12px] text-left hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
              >
                <div className="w-[24px] h-[24px] rounded-[6px] flex items-center justify-center flex-shrink-0 text-[var(--modal-text-secondary)]">
                  {expanded ? (
                    <ChevronDown className="w-[14px] h-[14px]" />
                  ) : (
                    <ChevronRight className="w-[14px] h-[14px]" />
                  )}
                </div>
                <div className="flex flex-col min-w-0 flex-1">
                  <h3 className="text-[14px] font-semibold text-[var(--modal-text-primary)] truncate">
                    {name}
                  </h3>
                  <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                    {skills.length}{" "}
                    {skills.length === 1 ? "skill" : "skills"}
                  </p>
                </div>
              </button>
              {expanded && (
                <div className="border-t border-[var(--modal-border-secondary)] bg-[var(--modal-bg-primary)] px-[12px] py-[8px] flex flex-col gap-[2px]">
                  {skills.map((skill) => (
                    <button
                      key={skill.id}
                      type="button"
                      onClick={() => onOpen(skill)}
                      className="flex items-center gap-[10px] px-[8px] py-[6px] rounded-[6px] hover:bg-[var(--modal-bg-hover)] text-left w-full cursor-pointer transition-colors"
                    >
                      <span className="text-[13px] text-[var(--modal-text-primary)] flex-1 truncate">
                        {skill.skill_name}
                      </span>
                      <span className="text-[11px] text-[var(--modal-text-tertiary)] font-mono truncate">
                        {skill.id}
                      </span>
                    </button>
                  ))}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

type IconComponent = typeof Puzzle;

function EmptyPane({
  icon: Icon,
  title,
  subtitle,
  action,
}: {
  icon: IconComponent;
  title: string;
  subtitle: string;
  action?: { label: string; onClick: () => void };
}) {
  return (
    <div className="flex-1 overflow-y-auto px-[24px] py-[20px]">
      <div className="flex flex-col items-center justify-center h-full text-center gap-[14px]">
        <Icon
          className="w-[44px] h-[44px] text-[var(--modal-text-tertiary)]"
          strokeWidth={1.5}
        />
        <div className="flex flex-col gap-[4px]">
          <p className="text-[15px] font-medium text-[var(--modal-text-primary)]">
            {title}
          </p>
          <p className="text-[13px] text-[var(--modal-text-tertiary)]">{subtitle}</p>
        </div>
        {action && (
          <button
            type="button"
            onClick={action.onClick}
            className="inline-flex items-center gap-[6px] h-[32px] px-[14px] rounded-[8px] text-[13px] font-medium text-white bg-[var(--modal-accent)] hover:opacity-90 transition-opacity cursor-pointer"
          >
            <Plus className="w-[14px] h-[14px]" />
            <span>{action.label}</span>
          </button>
        )}
      </div>
    </div>
  );
}

function LoadingPane() {
  return (
    <div className="flex-1 flex items-center justify-center">
      <div className="text-[13px] text-[var(--modal-text-tertiary)]">Loading…</div>
    </div>
  );
}

function ErrorPane({
  icon: Icon,
  message,
}: {
  icon: IconComponent;
  message: string;
}) {
  return (
    <div className="flex-1 overflow-y-auto px-[24px] py-[20px]">
      <div className="flex flex-col items-center justify-center h-full text-center gap-[14px]">
        <Icon
          className="w-[44px] h-[44px] text-[var(--modal-text-tertiary)]"
          strokeWidth={1.5}
        />
        <div className="flex flex-col gap-[4px]">
          <p className="text-[15px] font-medium text-[var(--modal-text-primary)]">
            Couldn't load
          </p>
          <p className="text-[13px] text-[var(--error)] max-w-[480px]">
            {message}
          </p>
        </div>
      </div>
    </div>
  );
}

// ===== Install plugin dialog =====

type InstallMode = "github" | "local";

function isLikelyGitHubUrl(raw: string): boolean {
  // Conservative client-side check: accept https://github.com/<owner>/<repo>
  // (with or without `.git`). The backend still validates — this just gives a
  // fast "URL looks malformed" signal for the obvious typos.
  const trimmed = raw.trim();
  if (!trimmed) return false;
  try {
    const url = new URL(trimmed);
    if (url.protocol !== "https:") return false;
    if (url.hostname !== "github.com" && url.hostname !== "www.github.com") {
      return false;
    }
    const parts = url.pathname.split("/").filter((p) => p.length > 0);
    return parts.length >= 2;
  } catch {
    return false;
  }
}

// Rewrite a GitHub blob/tree URL (a file/folder viewer URL) into the repo's
// clone URL plus an optional manifest-path override. `git clone` cannot accept
// a `…/blob/<branch>/<path>` URL — it appends a slash and reports "not found"
// — so we normalize before handing the URL to the backend.
function normalizeGitHubUrl(raw: string): {
  url: string;
  manifestPath: string | null;
  rewrote: boolean;
} {
  const trimmed = raw.trim();
  if (!trimmed) return { url: trimmed, manifestPath: null, rewrote: false };
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return { url: trimmed, manifestPath: null, rewrote: false };
  }
  if (url.hostname !== "github.com" && url.hostname !== "www.github.com") {
    return { url: trimmed, manifestPath: null, rewrote: false };
  }
  const parts = url.pathname.split("/").filter((p) => p.length > 0);
  // Need at least owner/repo/blob|tree/branch/<rest...> — i.e. 5 segments.
  if (parts.length < 5) return { url: trimmed, manifestPath: null, rewrote: false };
  const [owner, repo, kind, , ...rest] = parts;
  if (kind !== "blob" && kind !== "tree") {
    return { url: trimmed, manifestPath: null, rewrote: false };
  }
  const repoUrl = `https://github.com/${owner}/${repo}`;
  const path = rest.join("/");
  // Only treat the trailing path as a manifest override when it looks like a
  // JSON file. A `/tree/<branch>/some/dir` link points at a directory — we
  // strip it and let the user (or convention discovery) pick the manifest.
  const manifestPath = kind === "blob" && path.toLowerCase().endsWith(".json") ? path : null;
  return { url: repoUrl, manifestPath, rewrote: true };
}

function installErrorCopy(err: unknown): string {
  if (err && typeof err === "object" && "type" in err) {
    const e = err as PluginCommandError;
    switch (e.type) {
      case "conflict":
        return "Plugin already installed.";
      case "manifest_invalid":
        return `Manifest is invalid: ${e.detail}`;
      case "nothing_to_install":
        return "No manifest found and no skills/ or rules/ directory to auto-discover.";
      case "unsafe_name":
        return `Plugin name '${e.detail}' is not a safe folder name.`;
      case "source_missing":
        return `Source path does not exist: ${e.detail}`;
      case "network_error":
        return `Couldn't clone ${e.detail.url}: ${e.detail.detail}`;
      case "git_unavailable":
        return "git is not installed or not on PATH.";
      case "manifest_missing":
        // Caller handles this one specially (auto-discovery prompt); the
        // generic copy is only used if something drops the branch.
        return "No manifest found at the source.";
      default:
        return "detail" in e && typeof e.detail === "string"
          ? e.detail
          : e.type;
    }
  }
  if (err instanceof Error) return err.message;
  return String(err);
}

function InstallPluginDialog({
  open,
  onCancel,
  onInstalled,
}: {
  open: boolean;
  onCancel: () => void;
  onInstalled: (outcome: InstallOutcome) => void;
}) {
  const [mode, setMode] = useState<InstallMode>("github");
  const [urlInput, setUrlInput] = useState("");
  const [localPath, setLocalPath] = useState("");
  const [manifestOverride, setManifestOverride] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [manifestMissingPrompt, setManifestMissingPrompt] = useState(false);

  // Reset the whole dialog whenever it closes so reopening is a clean slate.
  useEffect(() => {
    if (!open) {
      setMode("github");
      setUrlInput("");
      setLocalPath("");
      setManifestOverride("");
      setSubmitting(false);
      setError(null);
      setManifestMissingPrompt(false);
    }
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !submitting) onCancel();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open, submitting, onCancel]);

  const pickFolder = useCallback(async () => {
    try {
      const selected = await tauriOpen({ directory: true, multiple: false });
      if (typeof selected === "string") setLocalPath(selected);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  // Returns the source and the manifest-override-to-use as a tuple so callers
  // don't have to reach back into React state — important when we just
  // rewrote a blob URL on this same tick (state updates haven't flushed yet).
  const buildSource = useCallback((): {
    source: PluginInstallSource;
    override: string | null;
  } | null => {
    if (mode === "github") {
      const trimmed = urlInput.trim();
      // Normalize blob/tree URLs (file/folder viewer URLs) into a clone URL +
      // optional manifest override. The user sees the rewrite reflected in
      // the form fields so it's not opaque.
      const { url: normalizedUrl, manifestPath, rewrote } = normalizeGitHubUrl(trimmed);
      const userOverride = manifestOverride.trim();
      const effectiveOverride = userOverride || (rewrote ? manifestPath : null) || null;
      if (rewrote) {
        if (normalizedUrl !== urlInput) setUrlInput(normalizedUrl);
        if (manifestPath && !userOverride) setManifestOverride(manifestPath);
      }
      if (!isLikelyGitHubUrl(normalizedUrl)) {
        setError("URL looks malformed. Expected https://github.com/<owner>/<repo>.");
        return null;
      }
      return {
        source: { type: "github_url", value: normalizedUrl },
        override: effectiveOverride,
      };
    }
    const trimmed = localPath.trim();
    if (!trimmed) {
      setError("Choose a folder to install from.");
      return null;
    }
    const override = manifestOverride.trim();
    return {
      source: { type: "local_path", value: trimmed },
      override: override ? override : null,
    };
  }, [mode, urlInput, localPath, manifestOverride]);

  const runInstall = useCallback(
    async (
      source: PluginInstallSource,
      override: string | null,
      allowAutoDiscovery: boolean,
    ) => {
      const outcome = await invoke<InstallOutcome>("install_plugin", {
        source,
        manifestOverride: override,
        allowAutoDiscovery,
      });
      return outcome;
    },
    [],
  );

  const handleSubmit = useCallback(async () => {
    setError(null);
    setManifestMissingPrompt(false);
    const built = buildSource();
    if (!built) return;

    setSubmitting(true);
    try {
      // First pass: let the backend surface ManifestMissing rather than
      // silently auto-discovering. If the user wants auto-discovery they'll
      // opt in via the prompt below.
      const outcome = await runInstall(built.source, built.override, false);
      onInstalled(outcome);
    } catch (err) {
      if (
        err &&
        typeof err === "object" &&
        "type" in err &&
        (err as PluginCommandError).type === "manifest_missing"
      ) {
        setManifestMissingPrompt(true);
      } else {
        setError(installErrorCopy(err));
      }
    } finally {
      setSubmitting(false);
    }
  }, [buildSource, runInstall, onInstalled]);

  const handleRetryWithAutoDiscovery = useCallback(async () => {
    setError(null);
    const built = buildSource();
    if (!built) {
      setManifestMissingPrompt(false);
      return;
    }
    setSubmitting(true);
    try {
      const outcome = await runInstall(built.source, built.override, true);
      onInstalled(outcome);
    } catch (err) {
      setManifestMissingPrompt(false);
      setError(installErrorCopy(err));
    } finally {
      setSubmitting(false);
    }
  }, [buildSource, runInstall, onInstalled]);

  const canSubmit =
    !submitting &&
    ((mode === "github" && urlInput.trim().length > 0) ||
      (mode === "local" && localPath.trim().length > 0));

  if (!open) return null;

  return createPortal(
    <AnimatePresence>
      <div
        className="fixed inset-0 z-[400] flex items-center justify-center"
        role="dialog"
        aria-modal="true"
        aria-labelledby="install-plugin-title"
      >
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.15 }}
          className="absolute inset-0 bg-black/40"
          onClick={() => {
            if (!submitting) onCancel();
          }}
        />
        <motion.div
          initial={{ opacity: 0, scale: 0.96 }}
          animate={{ opacity: 1, scale: 1 }}
          exit={{ opacity: 0, scale: 0.96 }}
          transition={{ duration: 0.15, ease: "easeOut" }}
          className="relative w-full max-w-[520px] rounded-[12px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)] flex flex-col"
          style={{
            boxShadow:
              "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)",
          }}
        >
          <div className="flex items-center justify-between px-[22px] pt-[18px] pb-[8px]">
            <h2
              id="install-plugin-title"
              className="text-[16px] font-semibold text-[var(--modal-text-primary)]"
            >
              Install plugin
            </h2>
            <button
              type="button"
              onClick={() => {
                if (!submitting) onCancel();
              }}
              disabled={submitting}
              aria-label="Close"
              className="w-[28px] h-[28px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
            >
              <X className="w-[16px] h-[16px]" />
            </button>
          </div>

          <div className="flex flex-col gap-[16px] px-[22px] pb-[18px]">
            <div
              className="flex items-center gap-[4px] p-[3px] rounded-[8px] bg-[var(--modal-bg-primary)] border border-[var(--modal-border-secondary)] w-fit"
              role="tablist"
              aria-label="Install source"
            >
              <ModeTab
                active={mode === "github"}
                label="GitHub URL"
                icon={Github}
                onClick={() => setMode("github")}
                disabled={submitting}
              />
              <ModeTab
                active={mode === "local"}
                label="Local Folder"
                icon={Folder}
                onClick={() => setMode("local")}
                disabled={submitting}
              />
            </div>

            {mode === "github" ? (
              <LabeledField label="Repository URL" htmlFor="install-github-url">
                <input
                  id="install-github-url"
                  type="text"
                  inputMode="url"
                  autoComplete="off"
                  spellCheck={false}
                  disabled={submitting}
                  value={urlInput}
                  onChange={(e) => setUrlInput(e.target.value)}
                  placeholder="https://github.com/owner/repo"
                  className="w-full h-[36px] px-[12px] rounded-[8px] bg-[var(--modal-bg-primary)] border border-[var(--modal-border-secondary)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] disabled:opacity-50"
                />
              </LabeledField>
            ) : (
              <LabeledField label="Folder" htmlFor="install-local-path">
                <div className="flex items-stretch gap-[8px]">
                  <input
                    id="install-local-path"
                    type="text"
                    readOnly
                    disabled={submitting}
                    value={localPath}
                    placeholder="No folder selected"
                    className="flex-1 h-[36px] px-[12px] rounded-[8px] bg-[var(--modal-bg-primary)] border border-[var(--modal-border-secondary)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none disabled:opacity-50"
                  />
                  <button
                    type="button"
                    onClick={pickFolder}
                    disabled={submitting}
                    className="inline-flex items-center gap-[6px] h-[36px] px-[12px] rounded-[8px] text-[13px] text-[var(--modal-text-primary)] bg-[var(--modal-bg-primary)] border border-[var(--modal-border-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:cursor-not-allowed disabled:opacity-50"
                  >
                    <FolderOpen className="w-[14px] h-[14px]" />
                    <span>Browse…</span>
                  </button>
                </div>
              </LabeledField>
            )}

            <LabeledField
              label="Manifest path override"
              htmlFor="install-manifest-override"
              hint="Optional. Relative path to a plugin.json inside the source."
            >
              <input
                id="install-manifest-override"
                type="text"
                autoComplete="off"
                spellCheck={false}
                disabled={submitting}
                value={manifestOverride}
                onChange={(e) => setManifestOverride(e.target.value)}
                placeholder="e.g. path/to/plugin.json"
                className="w-full h-[36px] px-[12px] rounded-[8px] bg-[var(--modal-bg-primary)] border border-[var(--modal-border-secondary)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] disabled:opacity-50"
              />
            </LabeledField>

            {error && (
              <div className="flex items-start gap-[8px] rounded-[8px] border border-[var(--error)] bg-[var(--modal-bg-primary)] px-[10px] py-[8px] text-[12px] text-[var(--error)]">
                <AlertTriangle className="w-[14px] h-[14px] mt-[2px] flex-shrink-0" />
                <span className="flex-1">{error}</span>
              </div>
            )}

            {manifestMissingPrompt && (
              <div className="flex flex-col gap-[10px] rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-primary)] px-[12px] py-[10px]">
                <div className="text-[13px] text-[var(--modal-text-primary)]">
                  No manifest found. Use auto-discovery?
                </div>
                <div className="text-[12px] text-[var(--modal-text-tertiary)]">
                  We'll import any top-level <code>skills/</code> and{" "}
                  <code>rules/</code> directories.
                </div>
                <div className="flex items-center justify-end gap-[8px]">
                  <button
                    type="button"
                    onClick={() => setManifestMissingPrompt(false)}
                    disabled={submitting}
                    className="h-[30px] px-[12px] rounded-[6px] text-[12px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                  >
                    No
                  </button>
                  <button
                    type="button"
                    onClick={handleRetryWithAutoDiscovery}
                    disabled={submitting}
                    className="inline-flex items-center gap-[6px] h-[30px] px-[12px] rounded-[6px] text-[12px] font-medium text-white bg-[var(--modal-accent)] hover:opacity-90 transition-opacity cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                  >
                    {submitting && (
                      <Loader2 className="w-[12px] h-[12px] animate-spin" />
                    )}
                    <span>Yes, auto-discover</span>
                  </button>
                </div>
              </div>
            )}
          </div>

          <div className="flex items-center justify-end gap-[10px] px-[22px] py-[14px] bg-[var(--modal-bg)] border-t border-[var(--modal-border-secondary)]">
            <button
              type="button"
              onClick={onCancel}
              disabled={submitting}
              className="h-[34px] px-[14px] rounded-[8px] text-[13px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={handleSubmit}
              disabled={!canSubmit}
              className="inline-flex items-center gap-[8px] h-[34px] px-[16px] rounded-[8px] text-[13px] font-semibold text-white bg-[var(--modal-accent)] hover:opacity-90 transition-opacity cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {submitting && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
              <span>Install</span>
            </button>
          </div>
        </motion.div>
      </div>
    </AnimatePresence>,
    document.body,
  );
}

function ModeTab({
  active,
  label,
  icon: Icon,
  onClick,
  disabled,
}: {
  active: boolean;
  label: string;
  icon: IconComponent;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      onClick={onClick}
      disabled={disabled}
      className={`inline-flex items-center gap-[6px] h-[28px] px-[10px] rounded-[6px] text-[12px] font-medium transition-colors cursor-pointer disabled:cursor-not-allowed disabled:opacity-50 ${active
        ? "bg-[var(--modal-bg)] text-[var(--modal-text-primary)] shadow-sm"
        : "text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)]"
        }`}
    >
      <Icon className="w-[13px] h-[13px]" />
      <span>{label}</span>
    </button>
  );
}

function LabeledField({
  label,
  htmlFor,
  hint,
  children,
}: {
  label: string;
  htmlFor: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-[6px]">
      <label
        htmlFor={htmlFor}
        className="text-[12px] font-medium text-[var(--modal-text-secondary)]"
      >
        {label}
      </label>
      {children}
      {hint && (
        <div className="text-[11px] text-[var(--modal-text-tertiary)]">{hint}</div>
      )}
    </div>
  );
}
