/**
 * Debug panel session unlock.
 *
 * The unlock state is kept in module scope (never persisted) so it
 * only lasts for the current browser / webview session.
 */

const DEBUG_EOL_DAYS = 100;

let unlocked = false;

export function isDebugUnlocked(): boolean {
  return unlocked;
}

export function setDebugUnlocked(value: boolean): void {
  unlocked = value;
}

/**
 * Compute the expected 6-digit debug code for a given version and date
 * using HMAC-SHA256 with the build secret.
 *
 * Algorithm matches `dev/generate-debug-code.sh`:
 *   HMAC-SHA256(version + YYYY-MM-DD, secret) → first 8 hex → decimal → mod 1e6 → zero-pad 6
 */
export async function computeDebugCode(
  version: string,
  date: string,
  secret: string,
): Promise<string | null> {
  if (!secret) return null;

  const encoder = new TextEncoder();
  const keyData = encoder.encode(secret);
  const message = encoder.encode(version + date);

  const key = await crypto.subtle.importKey(
    "raw",
    keyData,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );

  const signature = await crypto.subtle.sign("HMAC", key, message);
  const hex = Array.from(new Uint8Array(signature))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");

  // Take first 8 hex chars → decimal → mod 1000000 → zero-pad to 6 digits
  const decimal = parseInt(hex.slice(0, 8), 16);
  const code = String(decimal % 1000000).padStart(6, "0");
  return code;
}

/**
 * Check whether the debug panel has expired (100 days after build date).
 */
export function isDebugExpired(): boolean {
  const buildDate = import.meta.env.VITE_BUILD_DATE as string | undefined;
  if (!buildDate) return true; // No build date → treat as expired (silent fail)

  const built = new Date(buildDate + "T00:00:00Z");
  if (isNaN(built.getTime())) return true;

  const now = new Date();
  const diffMs = now.getTime() - built.getTime();
  const diffDays = diffMs / (1000 * 60 * 60 * 24);
  return diffDays > DEBUG_EOL_DAYS;
}

/**
 * Validate a user-entered debug code against the expected code for
 * the given version and today's date.
 *
 * Also enforces the 100-day EOL: if the build is older than 100 days,
 * validation silently fails.
 */
export async function validateDebugCode(
  code: string,
  version: string,
): Promise<boolean> {
  if (isDebugExpired()) return false;

  const secret = import.meta.env.VITE_BUILD_SECRET as string | undefined;
  if (!secret) return false;

  const today = new Date().toISOString().slice(0, 10); // YYYY-MM-DD

  const expected = await computeDebugCode(version, today, secret);
  if (!expected) return false;

  return code === expected;
}
