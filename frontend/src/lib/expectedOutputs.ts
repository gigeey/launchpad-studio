// Mirrors `OUTPUT_FILENAME_PREFIX_LEN` in `crates/ao-protocol/src/tasklist.rs`.
// Bump in lockstep if the backend constant changes.
const PREFIX_LEN = 8;
const PREFIX_RE = new RegExp(`^[A-Za-z0-9_-]{1,${PREFIX_LEN}}__`);

/**
 * Strip the per-task filename prefix the backend applies to every declared
 * output (see `prefix_expected_output` in ao-protocol). The prefix exists to
 * stop two parallel tasks from clobbering each other in the shared workspace,
 * but humans only need to see the base name in tiles, chips, and tooltips.
 *
 * Returns `filename` unchanged when no recognizable prefix is present.
 */
export function displayOutputFilename(filename: string): string {
  return filename.replace(PREFIX_RE, "");
}

/**
 * Hidden-output convention: any file whose base name starts with `_` is a
 * system-internal artifact that the outputs widget must not surface. Today
 * this exists for `_changelog.jsonl`, the per-tasklist append-only
 * log of `<task-item-notification>` payloads written by CLI agents. Future
 * system files written into the workspace should follow the same prefix
 * convention so this single check keeps them hidden.
 */
export function isHiddenOutput(filename: string): boolean {
  const base = filename.split("/").pop() ?? filename;
  return base.startsWith("_");
}

/** Filter a list of output filenames down to user-visible ones. */
export function filterVisibleOutputs(filenames: readonly string[]): string[] {
  return filenames.filter((f) => !isHiddenOutput(f));
}
