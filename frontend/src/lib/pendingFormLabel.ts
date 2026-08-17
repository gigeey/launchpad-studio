/** Label shown on a minimized pending-form pill (sync or async) — the
 *  asking agent's name is the primary information, per the "who is asking"
 *  requirement: a minimized pill that only repeats the form's own title
 *  gives no clue which agent it came from once more than one chat is in
 *  play. `fieldCount` drives the pluralized "N answers" — always at least 1
 *  even if a form somehow has zero fields, since "waiting on 0 answers"
 *  reads as nothing-to-do rather than the truth (the form itself is still
 *  pending). */
export function formatPendingFormWaitingLabel(
  agentName: string | null | undefined,
  fieldCount: number,
): string {
  const who = agentName?.trim() || "The agent";
  const count = Math.max(1, fieldCount);
  return `${who} is waiting on ${count} answer${count === 1 ? "" : "s"}`;
}
