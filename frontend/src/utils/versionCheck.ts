// Startup version gate — the app refuses to run when the local build is far
// enough behind the latest published release. Blocking a local-first tool from
// running is an unusual choice, so the rationale, the exact behavior of the
// threshold, and the alternatives that were rejected are all recorded below.
//
// WHY THERE IS A GATE AT ALL. The data directory only evolves forward, and
// nothing in the persistence layer is built to read it backwards. Every
// compatibility test there runs in one direction — a current build reading data
// an older one wrote: `distilled_through_ts_defaults_on_legacy_row` and
// `ensure_default_thread_is_non_destructive_to_existing_transcripts` in
// crates/ao-persistence/src/thread_store.rs, and
// `test_legacy_entry_without_updated_at_falls_back_to_created_at` in
// crates/ao-persistence/src/memory.rs. There is no version stamp on the data
// directory and no handling for the reverse case, so an old build meeting a
// directory a newer build has written is simply untested. Add live provider
// APIs that move underneath us, and a sufficiently old build fails in ways that
// are hard to attribute and easy to report as a bug in the current release.
// Past some distance, stopping is more honest than limping.
//
// THE SCOPE OF THAT ARGUMENT. It rests on the reverse direction being
// untested, not on a known break. No migration in the tree destroys or moves
// what it reads: new fields default when absent, and thread rows are
// materialized on demand rather than converted in a startup pass. One startup
// pass does still write — the `channel_origin` backfill in
// crates/ao-persistence/src/lib.rs stamps a field onto Slack bridge threads
// that predate it — but it only fills a value that was previously absent, and
// an older build ignores fields it does not know. So the risk is not that a
// newer build destroys an older build's data; it is that an older build reads
// a shape it has no code for. The gate is therefore precautionary, and
// MAX_VERSION_DISTANCE is a conservative guess rather than a measured limit.
//
// TODO(version-gate): re-derive MAX_VERSION_DISTANCE from evidence. Nothing
// tests an older build reading a data directory that a newer build has
// written, so the threshold is unmeasured. Add that test, then set the value
// from what it shows actually breaks — or drop the minor-version term and gate
// on major versions alone. Revisit sooner if a destructive migration is
// reintroduced.
//
// WHAT THE THRESHOLD ACTUALLY DOES. Distance is `major * 1000 + minor`; patch
// releases score zero. With MAX_VERSION_DISTANCE at 10 that is eleven minor
// releases behind — and it is ALSO any major-version gap whatsoever, because a
// single major bump scores 1000 on its own. Blocking across a major release is
// intended. The weighting that produces it is blunt, and it has one consequence
// that follows directly: on the day 1.0.0 ships, every build from a 0.x
// checkout blocks at launch and is told to download the official DMG. That is
// the designed outcome, not an oversight. The README section "What the app
// contacts" documents it so nobody meets it by surprise.
//
// IT FAILS OPEN, DELIBERATELY. When the fetch fails and nothing is cached,
// fetchLatestVersion returns null and App.tsx admits the user. Being offline
// must never be a reason to be locked out of your own machine. The cost is that
// the gate is advisory against anyone willing to block the request, which is
// the right trade: it exists to prevent accidental use of a stale build, not to
// enforce anything.
//
// WHY THIS FETCHES latest.json ITSELF INSTEAD OF ASKING THE UPDATER. The app
// also runs Tauri's updater plugin (stores/updateStore.ts), pointed at this
// same file, so the two look like the same request made twice and the obvious
// cleanup is to keep one. Doing that would delete the gate on most platforms.
// The two answer different questions, and only one of them can be answered
// everywhere:
//
//   - The gate asks "is this build too old to run?" It needs one string — the
//     `version` field — which says nothing about the machine reading it.
//   - The updater asks "can I install the newer build?" It needs a signed
//     artifact for the running target, and it resolves that artifact BEFORE it
//     compares versions, so on a target the manifest does not list it throws
//     rather than returning a version — up to date or not. The published
//     manifest carries darwin-aarch64 and darwin-x86_64 only. On Linux and
//     Windows the updater therefore cannot answer either question, while the
//     plain fetch below still answers this one.
//
// So the gate keeps its own request. The cost is one extra HTTPS GET per launch
// to a URL the app already contacts.
//
// ALTERNATIVE CONSIDERED — exempt builds that are not official releases. The
// marker exists: VITE_BUILD_SECRET is injected by the release pipeline and is
// undefined in a build from this repository (see lib/debugUnlock.ts), so the
// exemption would be a two-line change. It is not made, because the failure the
// gate protects against does not care where the binary came from — a stale
// source build meets a forward-written data directory in exactly the same way,
// and its owner has the cheapest possible fix, which is to pull and rebuild.
// Revisit if building from source becomes a common way to run the app rather
// than a way to develop it.
//
// THE NARROWER VERSION OF THAT EXEMPTION IS ALSO REJECTED — gating on
// `import.meta.env.DEV` so that only `tauri dev` sessions are waved through.
// It fails for the same reason, and more strongly: a developer switching
// branches is the
// single most likely person to run a stale build against a data directory a
// newer build has already written, so exempting dev would remove the
// protection exactly where the hazard is highest. What a dev build gets instead
// is the correct remedy rather than an exemption — ForceUpdateGate branches on
// DEV and says pull and rebuild, instead of offering the release DMG.
//
// The updater monitor in stores/updateStore.ts IS gated on production builds
// (see main.tsx). That is not an inconsistency: it answers the second of the
// two questions above, and installing a release bundle over a dev build is
// never a correct action, whereas refusing to run a stale one is.
export const MAX_VERSION_DISTANCE = 10;

function parseMajorMinor(version: string): { major: number; minor: number } {
  const cleaned = version.replace(/^v/, "");
  const [majorStr, minorStr] = cleaned.split(".");
  return {
    major: parseInt(majorStr, 10) || 0,
    minor: parseInt(minorStr, 10) || 0,
  };
}

export function calculateVersionDistance(
  current: string,
  latest: string
): number {
  const cur = parseMajorMinor(current);
  const lat = parseMajorMinor(latest);
  return lat.major * 1000 + lat.minor - (cur.major * 1000 + cur.minor);
}

export function isVersionTooOld(current: string, latest: string): boolean {
  return calculateVersionDistance(current, latest) > MAX_VERSION_DISTANCE;
}

// ---------------------------------------------------------------------------
// Latest version fetching & local caching
// ---------------------------------------------------------------------------

const LATEST_VERSION_URL =
  "https://github.com/gigeey/launchpad-studio-releases/releases/latest/download/latest.json";

const CACHE_KEY = "launchpad_latest_version";

const FETCH_TIMEOUT_MS = 3000;

interface CachedVersion {
  version: string;
  lastChecked: string; // ISO timestamp
}

function getCachedVersion(): CachedVersion | null {
  try {
    const raw = localStorage.getItem(CACHE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as CachedVersion;
    if (typeof parsed.version === "string" && typeof parsed.lastChecked === "string") {
      return parsed;
    }
    return null;
  } catch {
    return null;
  }
}

function setCachedVersion(version: string): void {
  const entry: CachedVersion = {
    version,
    lastChecked: new Date().toISOString(),
  };
  localStorage.setItem(CACHE_KEY, JSON.stringify(entry));
}

/**
 * Fetch the latest available version from the Tauri updater endpoint.
 * On success, caches the result in localStorage.
 * On failure, falls back to the cached version.
 * Returns `null` if both fetch and cache miss (fail open).
 */
export async function fetchLatestVersion(): Promise<string | null> {
  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);

    const response = await fetch(LATEST_VERSION_URL, {
      signal: controller.signal,
    });
    clearTimeout(timeoutId);

    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }

    const data = (await response.json()) as { version?: string };
    const version = data.version;

    if (typeof version === "string" && version.length > 0) {
      setCachedVersion(version);
      return version;
    }

    throw new Error("No version field in response");
  } catch {
    // Fallback to cache
    const cached = getCachedVersion();
    return cached?.version ?? null;
  }
}
