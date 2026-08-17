// Test-only fixture. Not imported by any shipped app code.
//
// Exact backend message text for the data-root ("workspace"/profile) API's
// failure responses. Centralized here — rather than inlined in each test
// assertion — so a future identifier/route/backend-string rename sweep can
// update these three strings in one place instead of hunting through every
// test file that asserts on them. Tests should build their mocked API
// rejections from these constants and assert against the *rendered* DOM
// output, never against a hardcoded UI-side copy of the same text — that
// way a real drift between what the backend sends and what the UI renders
// still fails the test.
export const WORKSPACE_API_MESSAGES = {
  /** POST /workspaces, 400 — path exists, is non-empty, and isn't already a
   *  registered Launchpad data root. */
  NOT_EMPTY_NOT_LAUNCHPAD:
    "That folder already has contents and isn't a Launchpad workspace. Choose an empty folder, or a folder that already contains a Launchpad workspace.",

  /** POST /workspaces, 400 — the path is already registered under another
   *  entry. Pre-existing message, unchanged this round. */
  registryCollision: (path: string) => `a workspace is already registered at ${path}`,

  /** POST /workspaces/{id}/activate, 409 — another running Launchpad Studio
   *  process already has this data root open. */
  activeElsewhere: (pid: number | string) =>
    `That workspace is in use by another running Launchpad Studio (pid ${pid}). Quit it first.`,

  /** POST /workspaces/{id}/activate, 400 — the pre-flight probe couldn't
   *  open the target workspace's data root (`AoError::
   *  WorkspaceActivationTargetUnopenable` in `ao-protocol/src/error.rs`).
   *  The registry pointer is never written when this fires. */
  activationTargetUnopenable: (path: string, cause: string) =>
    `Workspace data root could not be opened: ${path} (${cause})`,
} as const;
