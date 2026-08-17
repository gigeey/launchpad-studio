import { useEffect, useState } from "react";
import { listMcpServers } from "../../lib/api";
import type { McpServerStatus } from "../../types/connectors";
import type { AssignmentBinding } from "../../types/api";

/** Convention-only literal (no backend enforcement yet): every MCP-server
 *  binding this picker produces is tagged with this `kind`. Later
 *  connector-event trigger work must reuse the same string so existing
 *  bindings stay recognizable once the backend starts interpreting `kind`. */
export const MCP_SERVER_BINDING_KIND = "mcp_server";

interface BindingPickerProps {
  bindings: AssignmentBinding[];
  onChange: (bindings: AssignmentBinding[]) => void;
}

/** Lets an assignment declare which connected MCP servers it's expected to
 *  "use" — purely descriptive metadata today (bindings aren't enforced
 *  server-side), surfaced so a later phase can wire actual access control or
 *  connector-event triggers off the same list. */
export function BindingPicker({ bindings, onChange }: BindingPickerProps) {
  const [servers, setServers] = useState<McpServerStatus[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    listMcpServers()
      .then((list) => {
        if (!cancelled) setServers(list);
      })
      .catch(() => {
        if (!cancelled) setServers([]);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const connected = servers.filter((s) => s.state === "connected");
  const selectedNames = new Set(
    bindings.filter((b) => b.kind === MCP_SERVER_BINDING_KIND).map((b) => b.ref_id),
  );

  const toggle = (name: string) => {
    if (selectedNames.has(name)) {
      onChange(bindings.filter((b) => !(b.kind === MCP_SERVER_BINDING_KIND && b.ref_id === name)));
    } else {
      onChange([...bindings, { kind: MCP_SERVER_BINDING_KIND, ref_id: name }]);
    }
  };

  if (loading) {
    return (
      <div className="text-[12px] text-[var(--modal-text-secondary)]" data-testid="binding-picker-loading">
        Loading connectors…
      </div>
    );
  }

  if (connected.length === 0) {
    return (
      <div
        className="text-[12px] text-[var(--modal-text-secondary)] italic"
        data-testid="binding-picker-empty"
      >
        No connected connectors yet — add one from Connectors settings to bind it here.
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-[6px]" data-testid="binding-picker">
      {connected.map((server) => (
        <label
          key={server.name}
          className="inline-flex items-center gap-[8px] text-[13px] text-[var(--modal-text-primary)] cursor-pointer"
        >
          <input
            type="checkbox"
            checked={selectedNames.has(server.name)}
            onChange={() => toggle(server.name)}
            data-testid={`binding-picker-checkbox-${server.name}`}
          />
          {server.name}
        </label>
      ))}
    </div>
  );
}
