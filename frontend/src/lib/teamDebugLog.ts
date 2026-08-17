// ---------------------------------------------------------------------------
// Debug logger for team SSE events.
// Writes to /tmp/team_debug.log via the backend SSE stream handler instead.
// This frontend version just logs to console for reference.
// ---------------------------------------------------------------------------

// eslint-disable-next-line @typescript-eslint/no-unused-vars
export function teamLog(_src: string, _detail?: string) {
  // no-op: logging is done on the backend side (stream.rs -> /tmp/team_debug.log)
}

export function formatTeamLog(): string {
  return "(see /tmp/team_debug.log for SSE event log)";
}
