export type McpServerState = "connected" | "needs_auth" | "error" | "disabled";

export interface McpServerStatus {
  name: string;
  transport: string;
  endpoint: string;
  state: McpServerState;
  error?: string | null;
  tool_names: string[];
  source: string;
}

export interface AddMcpServerRequest {
  name: string;
  transport: "stdio" | "http";
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
}
