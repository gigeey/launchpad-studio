// Defensive shape-checking helpers for typed-artifact payloads.
//
// The authoring tool (ArtifactWrite) only enforces "payload is a JSON object
// for typed kinds" — it does not (and per the render contract, should not)
// lock down a per-kind schema. So every typed renderer below is written to
// degrade to a raw-JSON fallback rather than throw when a payload doesn't
// match the shape it expects. This keeps a malformed-but-well-kinded payload
// in the same "never blank-screen" posture as an unrecognized `kind`.

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

export function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/** Renders a scalar/object cell value as display text. Objects/arrays get
 *  JSON-stringified rather than "[object Object]"-ed. */
export function formatCell(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

/** Locates the first array a typed-renderer container can plausibly use,
 *  trying each candidate key against the payload record in order. LLM-authored
 *  payloads name the same concept ("items" vs "cards" vs "data") differently
 *  across generations, so renderers accept a short list of synonyms instead of
 *  one fixed key. Returns null when `payload` isn't a record or none of the
 *  keys hold an array. */
export function firstArray(payload: unknown, keys: string[]): unknown[] | null {
  if (!isRecord(payload)) return null;
  for (const key of keys) {
    const value = payload[key];
    if (Array.isArray(value)) return value;
  }
  return null;
}

/** Reads the first string-valued field present on a record, trying each
 *  candidate key in order. Used to accept synonym field names (e.g. a card
 *  item using "name" where the renderer's primary field is "title"). */
export function firstString(record: Record<string, unknown>, keys: string[]): string | undefined {
  for (const key of keys) {
    const value = asString(record[key]);
    if (value !== undefined) return value;
  }
  return undefined;
}
