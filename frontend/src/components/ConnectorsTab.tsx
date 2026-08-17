import { useCallback, useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { createPortal } from "react-dom";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  AlertTriangle,
  Cable,
  Check,
  ChevronDown,
  CircleOff,
  KeyRound,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
  X,
} from "lucide-react";
import { listMcpServers, addMcpServer, deleteMcpServer, authorizeMcpServer } from "../lib/api";
import ConfirmDialog from "./ui/ConfirmDialog";
import type { McpServerStatus, AddMcpServerRequest } from "../types/connectors";

type LoadState<T> =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ready"; data: T }
  | { status: "error"; message: string };

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}

// ── ConnectorsTab ─────────────────────────────────────────────────────────────

export function ConnectorsTab({
  state,
  onRefresh,
  onRequestAdd,
}: {
  state: LoadState<McpServerStatus[]>;
  onRefresh: () => void;
  onRequestAdd: () => void;
}) {
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  const handleConfirmDelete = useCallback(async () => {
    if (!deleteTarget) return;
    try {
      await deleteMcpServer(deleteTarget);
      setDeleteTarget(null);
      onRefresh();
    } catch (err) {
      setDeleteError(errorMessage(err));
      setDeleteTarget(null);
    }
  }, [deleteTarget, onRefresh]);

  if (state.status === "loading" || state.status === "idle") {
    return (
      <div className="flex-1 flex items-center justify-center">
        <div className="text-[13px] text-[var(--text-tertiary)]">Loading…</div>
      </div>
    );
  }

  if (state.status === "error") {
    return (
      <div className="flex-1 flex items-center justify-center px-[24px] py-[20px]">
        <div className="flex flex-col items-center gap-[14px] text-center">
          <Cable className="w-[44px] h-[44px] text-[var(--text-tertiary)]" strokeWidth={1.5} />
          <p className="text-[15px] font-medium text-[var(--text-primary)]">Couldn't load</p>
          <p className="text-[13px] text-[var(--error)] max-w-[480px]">{state.message}</p>
        </div>
      </div>
    );
  }

  const servers = state.data;

  return (
    <div className="flex-1 flex flex-col min-h-0">
      <div className="flex items-center justify-between px-[24px] pt-[16px] pb-[8px]">
        <p className="text-[12px] text-[var(--text-tertiary)]">
          {servers.length === 0 ? "No connectors configured" : `${servers.length} connector${servers.length === 1 ? "" : "s"}`}
        </p>
        <button
          type="button"
          onClick={onRequestAdd}
          className="inline-flex items-center gap-[6px] h-[30px] px-[12px] rounded-[8px] text-[13px] font-medium text-white bg-[var(--accent)] hover:opacity-90 transition-opacity cursor-pointer"
        >
          <Plus className="w-[14px] h-[14px]" />
          <span>Add connector</span>
        </button>
      </div>

      {servers.length === 0 ? (
        <div className="flex-1 flex items-center justify-center px-[24px] py-[20px]">
          <div className="flex flex-col items-center gap-[14px] text-center">
            <Cable className="w-[44px] h-[44px] text-[var(--text-tertiary)]" strokeWidth={1.5} />
            <p className="text-[15px] font-medium text-[var(--text-primary)]">No connectors yet</p>
            <p className="text-[13px] text-[var(--text-tertiary)]">Add an MCP server to get started.</p>
            <button
              type="button"
              onClick={onRequestAdd}
              className="inline-flex items-center gap-[6px] h-[32px] px-[14px] rounded-[8px] text-[13px] font-medium text-white bg-[var(--accent)] hover:opacity-90 transition-opacity cursor-pointer"
            >
              <Plus className="w-[14px] h-[14px]" />
              <span>Add connector</span>
            </button>
          </div>
        </div>
      ) : (
        <div className="flex-1 overflow-y-auto px-[24px] py-[8px] pb-[16px]">
          <div className="grid grid-cols-2 gap-[14px] auto-rows-min">
            {servers.map((server) => (
              <ConnectorCard
                key={server.name}
                server={server}
                onRequestDelete={setDeleteTarget}
                onRefresh={onRefresh}
              />
            ))}
          </div>
        </div>
      )}

      {deleteError && (
        <div className="absolute bottom-[16px] left-[24px] right-[24px] z-[10] flex items-center gap-[8px] rounded-[10px] border border-[var(--error)] bg-[var(--bg-secondary)] px-[12px] py-[8px] text-[12px] text-[var(--error)] shadow-lg">
          <AlertTriangle className="w-[14px] h-[14px] flex-shrink-0" />
          <span className="flex-1">{deleteError}</span>
          <button type="button" onClick={() => setDeleteError(null)} className="cursor-pointer">
            <X className="w-[12px] h-[12px]" />
          </button>
        </div>
      )}

      <ConfirmDialog
        open={deleteTarget !== null}
        title="Remove connector"
        message={
          deleteTarget ? (
            <>
              Remove <span className="font-semibold text-[var(--text-primary)]">{deleteTarget}</span>
              {" "}from connectors? Its tools will be unregistered immediately.
            </>
          ) : ""
        }
        confirmLabel="Remove"
        destructive
        onConfirm={handleConfirmDelete}
        onCancel={() => setDeleteTarget(null)}
      />
    </div>
  );
}

// ── ConnectorCard ─────────────────────────────────────────────────────────────

const AUTHORIZE_POLL_INTERVAL_MS = 2000;
const AUTHORIZE_MAX_POLLS = 60; // ~2 minutes

function ConnectorCard({
  server,
  onRequestDelete,
  onRefresh,
}: {
  server: McpServerStatus;
  onRequestDelete: (name: string) => void;
  onRefresh: () => void;
}) {
  const [authorizing, setAuthorizing] = useState(false);
  const [authError, setAuthError] = useState<string | null>(null);
  const [errorExpanded, setErrorExpanded] = useState(false);
  const pollCountRef = useRef(0);
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const isPluginSource = server.source.startsWith("plugin:");
  const pluginLabel = isPluginSource ? server.source.slice("plugin:".length) : null;

  const stopPolling = useCallback(() => {
    if (pollTimerRef.current !== null) {
      clearTimeout(pollTimerRef.current);
      pollTimerRef.current = null;
    }
  }, []);

  useEffect(() => () => stopPolling(), [stopPolling]);

  const startPolling = useCallback(() => {
    pollCountRef.current = 0;

    const poll = async () => {
      if (pollCountRef.current >= AUTHORIZE_MAX_POLLS) {
        setAuthorizing(false);
        setAuthError("Authorization timed out. Refresh to check status.");
        return;
      }
      pollCountRef.current += 1;

      try {
        const servers = await listMcpServers();
        const updated = servers.find((s) => s.name === server.name);
        if (updated?.state === "connected") {
          setAuthorizing(false);
          onRefresh();
          return;
        }
        if (updated?.state === "error") {
          setAuthorizing(false);
          setAuthError(updated.error ?? "Authorization failed.");
          return;
        }
      } catch {
        // network error — keep polling
      }

      pollTimerRef.current = setTimeout(poll, AUTHORIZE_POLL_INTERVAL_MS);
    };

    pollTimerRef.current = setTimeout(poll, AUTHORIZE_POLL_INTERVAL_MS);
  }, [server.name, onRefresh]);

  const handleAuthorize = useCallback(async () => {
    setAuthError(null);
    setAuthorizing(true);
    // A server that's already `connected` has no needs_auth → connected
    // transition to poll for — reauthorizing it rotates the credential in
    // place, so `state` never changes. Polling for that non-event would just
    // report false success as soon as the first poll ran.
    const alreadyConnected = server.state === "connected";
    try {
      const { auth_url } = await authorizeMcpServer(server.name);
      // Tauri webviews ignore window.open for external URLs — route through the
      // system browser via the opener plugin (same path main.tsx uses for links).
      await openUrl(auth_url);
      if (alreadyConnected) {
        setAuthorizing(false);
      } else {
        startPolling();
      }
    } catch (err) {
      setAuthorizing(false);
      setAuthError(errorMessage(err));
    }
  }, [server.name, server.state, startPolling]);

  const canReauthorize =
    server.transport !== "stdio" && server.state !== "needs_auth" && server.state !== "disabled";

  const { badge, badgeText } = stateDisplay(server.state);

  return (
    <div className="rounded-[14px] border border-[var(--border-secondary)] bg-[var(--bg-secondary)] px-[16px] py-[14px] flex flex-col gap-[8px]">
      <div className="flex items-start justify-between gap-[12px]">
        <div className="flex items-center gap-[8px] min-w-0 flex-1">
          <h3 className="text-[15px] font-semibold text-[var(--text-primary)] truncate" title={server.name}>
            {server.name}
          </h3>
          <span className="text-[11px] font-medium px-[6px] py-[2px] rounded-[4px] bg-[var(--bg-primary)] text-[var(--text-tertiary)] border border-[var(--border-secondary)] flex-shrink-0">
            {server.transport}
          </span>
        </div>
        <div className="flex items-center gap-[2px] flex-shrink-0">
          {canReauthorize && (
            <button
              type="button"
              onClick={handleAuthorize}
              disabled={authorizing}
              aria-label={`Reauthorize ${server.name}`}
              title="Reauthorize"
              className="h-[28px] w-[28px] rounded-[6px] flex items-center justify-center text-[var(--text-tertiary)] hover:text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {authorizing ? (
                <Loader2 className="w-[13px] h-[13px] animate-spin" />
              ) : (
                <RefreshCw className="w-[13px] h-[13px]" />
              )}
            </button>
          )}
          {!isPluginSource && (
            <button
              type="button"
              onClick={() => onRequestDelete(server.name)}
              aria-label={`Remove ${server.name}`}
              title="Remove"
              className="h-[28px] w-[28px] rounded-[6px] flex items-center justify-center text-[var(--text-secondary)] hover:text-[var(--error)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer flex-shrink-0"
            >
              <Trash2 className="w-[14px] h-[14px]" />
            </button>
          )}
        </div>
      </div>

      <p className="text-[12px] text-[var(--text-secondary)] truncate" title={server.endpoint}>
        {server.endpoint || "—"}
      </p>

      <div className="border-t border-[var(--border-secondary)] -mx-[16px]" />

      <div className="flex items-center justify-between gap-[8px] flex-wrap">
        <div className="flex items-center gap-[10px]">
          <span className={`inline-flex items-center gap-[5px] text-[12px] font-medium px-[8px] py-[3px] rounded-[6px] ${badge}`}>
            {badgeIcon(server.state)}
            {badgeText}
          </span>
          {server.tool_names.length > 0 && (
            <span className="text-[12px] text-[var(--text-tertiary)]">
              {server.tool_names.length} tool{server.tool_names.length === 1 ? "" : "s"}
            </span>
          )}
        </div>

        <div className="flex items-center gap-[8px]">
          {isPluginSource && pluginLabel && (
            <span className="text-[11px] text-[var(--text-tertiary)]">via {pluginLabel}</span>
          )}

          {server.state === "needs_auth" && (
            <button
              type="button"
              onClick={handleAuthorize}
              disabled={authorizing}
              className="inline-flex items-center gap-[5px] h-[26px] px-[10px] rounded-[6px] text-[12px] font-medium bg-amber-500/15 text-amber-600 hover:bg-amber-500/25 transition-colors cursor-pointer disabled:opacity-60 disabled:cursor-not-allowed"
            >
              {authorizing ? (
                <Loader2 className="w-[11px] h-[11px] animate-spin" />
              ) : (
                <KeyRound className="w-[11px] h-[11px]" />
              )}
              <span>{authorizing ? "Authorizing…" : "Authorize"}</span>
            </button>
          )}
        </div>
      </div>

      {(server.state === "error" && server.error) && (
        <div className="flex flex-col gap-[4px]">
          <button
            type="button"
            onClick={() => setErrorExpanded((v) => !v)}
            className="flex items-center gap-[6px] text-[12px] text-[var(--error)] cursor-pointer hover:opacity-80 transition-opacity"
          >
            <ChevronDown className={`w-[12px] h-[12px] transition-transform ${errorExpanded ? "rotate-180" : ""}`} />
            <span>{errorExpanded ? "Hide error" : "Show error"}</span>
          </button>
          {errorExpanded && (
            <p className="text-[11px] text-[var(--error)] bg-[var(--bg-primary)] rounded-[6px] px-[8px] py-[6px] font-mono break-all">
              {server.error}
            </p>
          )}
        </div>
      )}

      {authError && (
        <div className="flex items-start gap-[8px] rounded-[8px] border border-[var(--error)] bg-[var(--bg-primary)] px-[10px] py-[8px] text-[12px] text-[var(--error)]">
          <AlertTriangle className="w-[12px] h-[12px] mt-[2px] flex-shrink-0" />
          <span className="flex-1">{authError}</span>
          <button type="button" onClick={() => setAuthError(null)} className="cursor-pointer">
            <X className="w-[12px] h-[12px]" />
          </button>
        </div>
      )}
    </div>
  );
}

function stateDisplay(state: McpServerStatus["state"]): { badge: string; badgeText: string } {
  switch (state) {
    case "connected":
      return { badge: "bg-green-500/15 text-green-600", badgeText: "Connected" };
    case "needs_auth":
      return { badge: "bg-amber-500/15 text-amber-600", badgeText: "Needs auth" };
    case "error":
      return { badge: "bg-red-500/15 text-[var(--error)]", badgeText: "Error" };
    case "disabled":
      return { badge: "bg-[var(--bg-hover)] text-[var(--text-tertiary)]", badgeText: "Disabled" };
  }
}

function badgeIcon(state: McpServerStatus["state"]) {
  switch (state) {
    case "connected":
      return <Check className="w-[11px] h-[11px]" />;
    case "needs_auth":
      return <KeyRound className="w-[11px] h-[11px]" />;
    case "error":
      return <AlertTriangle className="w-[11px] h-[11px]" />;
    case "disabled":
      return <CircleOff className="w-[11px] h-[11px]" />;
  }
}

// ── AddConnectorDialog ────────────────────────────────────────────────────────

type TransportMode = "stdio" | "http";

interface EnvRow {
  key: string;
  value: string;
}

export function AddConnectorDialog({
  open,
  onCancel,
  onAdded,
}: {
  open: boolean;
  onCancel: () => void;
  onAdded: () => void;
}) {
  const [name, setName] = useState("");
  const [transport, setTransport] = useState<TransportMode>("stdio");
  const [command, setCommand] = useState("");
  const [argsText, setArgsText] = useState("");
  const [url, setUrl] = useState("");
  const [envRows, setEnvRows] = useState<EnvRow[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) {
      setName("");
      setTransport("stdio");
      setCommand("");
      setArgsText("");
      setUrl("");
      setEnvRows([]);
      setSubmitting(false);
      setError(null);
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

  const addEnvRow = () => setEnvRows((r) => [...r, { key: "", value: "" }]);
  const removeEnvRow = (i: number) => setEnvRows((r) => r.filter((_, idx) => idx !== i));
  const updateEnvRow = (i: number, field: "key" | "value", val: string) =>
    setEnvRows((r) => r.map((row, idx) => (idx === i ? { ...row, [field]: val } : row)));

  const handleSubmit = useCallback(async () => {
    setError(null);
    const trimmedName = name.trim();
    if (!trimmedName) { setError("Name is required."); return; }
    if (transport === "stdio" && !command.trim()) { setError("Command is required for stdio transport."); return; }
    if (transport === "http" && !url.trim()) { setError("URL is required for HTTP transport."); return; }

    const env: Record<string, string> = {};
    for (const row of envRows) {
      const k = row.key.trim();
      const v = row.value.trim();
      if (k) env[k] = v;
    }

    const req: AddMcpServerRequest = { name: trimmedName, transport };
    if (transport === "stdio") {
      req.command = command.trim();
      const args = argsText.split("\n").map((l) => l.trim()).filter(Boolean);
      if (args.length > 0) req.args = args;
    } else {
      req.url = url.trim();
    }
    if (Object.keys(env).length > 0) req.env = env;

    setSubmitting(true);
    try {
      await addMcpServer(req);
      onAdded();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setSubmitting(false);
    }
  }, [name, transport, command, argsText, url, envRows, onAdded]);

  const canSubmit = !submitting && name.trim().length > 0 &&
    ((transport === "stdio" && command.trim().length > 0) ||
      (transport === "http" && url.trim().length > 0));

  if (!open) return null;

  return createPortal(
    <AnimatePresence>
      <div
        className="fixed inset-0 z-[400] flex items-center justify-center"
        role="dialog"
        aria-modal="true"
        aria-labelledby="add-connector-title"
      >
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.15 }}
          className="absolute inset-0 bg-black/40"
          onClick={() => { if (!submitting) onCancel(); }}
        />
        <motion.div
          initial={{ opacity: 0, scale: 0.96 }}
          animate={{ opacity: 1, scale: 1 }}
          exit={{ opacity: 0, scale: 0.96 }}
          transition={{ duration: 0.15, ease: "easeOut" }}
          className="relative w-full max-w-[520px] max-h-[85vh] overflow-y-auto rounded-[12px] bg-[var(--bg-secondary)] border border-[var(--border-secondary)] flex flex-col"
          style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
        >
          <div className="flex items-center justify-between px-[22px] pt-[18px] pb-[8px]">
            <h2 id="add-connector-title" className="text-[16px] font-semibold text-[var(--text-primary)]">
              Add connector
            </h2>
            <button
              type="button"
              onClick={() => { if (!submitting) onCancel(); }}
              disabled={submitting}
              aria-label="Close"
              className="w-[28px] h-[28px] rounded-[6px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
            >
              <X className="w-[16px] h-[16px]" />
            </button>
          </div>

          <div className="flex flex-col gap-[16px] px-[22px] pb-[18px]">
            <ConnectorField label="Name" htmlFor="connector-name" hint="Lowercase letters, digits, and underscores. Must start with a letter.">
              <input
                id="connector-name"
                type="text"
                autoComplete="off"
                spellCheck={false}
                disabled={submitting}
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="e.g. github"
                className="w-full h-[36px] px-[12px] rounded-[8px] bg-[var(--bg-primary)] border border-[var(--border-secondary)] text-[13px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] disabled:opacity-50"
              />
            </ConnectorField>

            <ConnectorField label="Transport" htmlFor="connector-transport">
              <div
                className="flex items-center gap-[4px] p-[3px] rounded-[8px] bg-[var(--bg-primary)] border border-[var(--border-secondary)] w-fit"
                role="group"
              >
                <TransportTab active={transport === "stdio"} label="stdio" onClick={() => setTransport("stdio")} disabled={submitting} />
                <TransportTab active={transport === "http"} label="HTTP" onClick={() => setTransport("http")} disabled={submitting} />
              </div>
            </ConnectorField>

            {transport === "stdio" ? (
              <>
                <ConnectorField label="Command" htmlFor="connector-command">
                  <input
                    id="connector-command"
                    type="text"
                    autoComplete="off"
                    spellCheck={false}
                    disabled={submitting}
                    value={command}
                    onChange={(e) => setCommand(e.target.value)}
                    placeholder="e.g. npx -y @modelcontextprotocol/server-github"
                    className="w-full h-[36px] px-[12px] rounded-[8px] bg-[var(--bg-primary)] border border-[var(--border-secondary)] text-[13px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] disabled:opacity-50"
                  />
                </ConnectorField>

                <ConnectorField
                  label="Arguments"
                  htmlFor="connector-args"
                  hint="One argument per line."
                >
                  <textarea
                    id="connector-args"
                    disabled={submitting}
                    value={argsText}
                    onChange={(e) => setArgsText(e.target.value)}
                    placeholder={"--verbose\n--port 3000"}
                    rows={3}
                    className="w-full px-[12px] py-[8px] rounded-[8px] bg-[var(--bg-primary)] border border-[var(--border-secondary)] text-[13px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] disabled:opacity-50 resize-none font-mono"
                  />
                </ConnectorField>
              </>
            ) : (
              <ConnectorField label="URL" htmlFor="connector-url">
                <input
                  id="connector-url"
                  type="text"
                  inputMode="url"
                  autoComplete="off"
                  spellCheck={false}
                  disabled={submitting}
                  value={url}
                  onChange={(e) => setUrl(e.target.value)}
                  placeholder="https://example.com/mcp"
                  className="w-full h-[36px] px-[12px] rounded-[8px] bg-[var(--bg-primary)] border border-[var(--border-secondary)] text-[13px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] disabled:opacity-50"
                />
              </ConnectorField>
            )}

            <div className="flex flex-col gap-[8px]">
              <div className="flex items-center justify-between">
                <span className="text-[12px] font-medium text-[var(--text-secondary)]">Environment variables</span>
                <button
                  type="button"
                  onClick={addEnvRow}
                  disabled={submitting}
                  className="inline-flex items-center gap-[4px] text-[12px] text-[var(--accent)] hover:opacity-80 transition-opacity cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  <Plus className="w-[12px] h-[12px]" />
                  <span>Add</span>
                </button>
              </div>
              {envRows.length > 0 && (
                <div className="flex flex-col gap-[6px]">
                  {envRows.map((row, i) => (
                    <div key={i} className="flex items-center gap-[6px]">
                      <input
                        type="text"
                        disabled={submitting}
                        value={row.key}
                        onChange={(e) => updateEnvRow(i, "key", e.target.value)}
                        placeholder="KEY"
                        className="w-[140px] h-[32px] px-[10px] rounded-[6px] bg-[var(--bg-primary)] border border-[var(--border-secondary)] text-[12px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] disabled:opacity-50 font-mono"
                      />
                      <span className="text-[var(--text-tertiary)] text-[12px]">=</span>
                      <input
                        type="text"
                        disabled={submitting}
                        value={row.value}
                        onChange={(e) => updateEnvRow(i, "value", e.target.value)}
                        placeholder="value"
                        className="flex-1 h-[32px] px-[10px] rounded-[6px] bg-[var(--bg-primary)] border border-[var(--border-secondary)] text-[12px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] disabled:opacity-50 font-mono"
                      />
                      <button
                        type="button"
                        onClick={() => removeEnvRow(i)}
                        disabled={submitting}
                        className="w-[26px] h-[26px] rounded-[5px] flex items-center justify-center text-[var(--text-secondary)] hover:text-[var(--error)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                      >
                        <X className="w-[12px] h-[12px]" />
                      </button>
                    </div>
                  ))}
                </div>
              )}
            </div>

            {error && (
              <div className="flex items-start gap-[8px] rounded-[8px] border border-[var(--error)] bg-[var(--bg-primary)] px-[10px] py-[8px] text-[12px] text-[var(--error)]">
                <AlertTriangle className="w-[14px] h-[14px] mt-[2px] flex-shrink-0" />
                <span className="flex-1">{error}</span>
              </div>
            )}
          </div>

          <div className="flex items-center justify-end gap-[10px] px-[22px] py-[14px] bg-[var(--bg-secondary)] border-t border-[var(--border-secondary)] sticky bottom-0">
            <button
              type="button"
              onClick={onCancel}
              disabled={submitting}
              className="h-[34px] px-[14px] rounded-[8px] text-[13px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={handleSubmit}
              disabled={!canSubmit}
              className="inline-flex items-center gap-[8px] h-[34px] px-[16px] rounded-[8px] text-[13px] font-semibold text-white bg-[var(--accent)] hover:opacity-90 transition-opacity cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {submitting && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
              <span>Add</span>
            </button>
          </div>
        </motion.div>
      </div>
    </AnimatePresence>,
    document.body,
  );
}

function TransportTab({
  active,
  label,
  onClick,
  disabled,
}: {
  active: boolean;
  label: string;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={`h-[28px] px-[10px] rounded-[6px] text-[12px] font-medium transition-colors cursor-pointer disabled:cursor-not-allowed disabled:opacity-50 ${
        active
          ? "bg-[var(--bg-secondary)] text-[var(--text-primary)] shadow-sm"
          : "text-[var(--text-secondary)] hover:text-[var(--text-primary)]"
      }`}
    >
      {label}
    </button>
  );
}

function ConnectorField({
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
      <label htmlFor={htmlFor} className="text-[12px] font-medium text-[var(--text-secondary)]">
        {label}
      </label>
      {children}
      {hint && <div className="text-[11px] text-[var(--text-tertiary)]">{hint}</div>}
    </div>
  );
}
