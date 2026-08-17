import { create } from "zustand";
import { isHubRecentlyAlive } from "../lib/sseHub";

const HEALTH_URL =
  (import.meta.env.VITE_API_BASE_URL ?? "http://localhost:3001") + "/health";

/** How often to ping the backend (ms) */
const SERVER_PING_INTERVAL = 10_000;

/** How often to check internet connectivity (ms) */
const INTERNET_CHECK_INTERVAL = 10_000;

/** Timeout for a single request (ms) */
const REQUEST_TIMEOUT = 5_000;

/** Consecutive failed /health pings required before the banner flips
 *  `isServerOnline` to false. A single transient failure (e.g. the fetch
 *  briefly racing the browser's connection pool) no longer trips it. */
const OFFLINE_FAILURE_THRESHOLD = 2;

/** Google's connectivity check — returns 204, tiny payload */
const INTERNET_CHECK_URL = "https://www.google.com/generate_204";

interface NetworkState {
  isInternetOnline: boolean;
  isServerOnline: boolean;
}

export const useNetworkStore = create<NetworkState>(() => ({
  isInternetOnline: navigator.onLine,
  isServerOnline: true, // assume up until first check
}));

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

let serverIntervalId: ReturnType<typeof setInterval> | null = null;
let internetIntervalId: ReturnType<typeof setInterval> | null = null;

async function pingServer(): Promise<boolean> {
  // The hub's single `/system/stream` connection proves the server is up
  // whenever it has heard from it recently, so skip the request entirely
  // rather than racing this fetch() against the browser's per-host
  // connection pool while other SSE traffic is active.
  if (isHubRecentlyAlive()) return true;
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT);
    const res = await fetch(HEALTH_URL, {
      method: "GET",
      signal: controller.signal,
      cache: "no-store",
    });
    clearTimeout(timer);
    return res.ok;
  } catch {
    return false;
  }
}

async function checkInternet(): Promise<boolean> {
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT);
    await fetch(INTERNET_CHECK_URL, {
      method: "HEAD",
      mode: "no-cors",
      signal: controller.signal,
      cache: "no-store",
    });
    clearTimeout(timer);
    // no-cors returns opaque response (status 0) but that's fine — if fetch
    // didn't throw, we have connectivity.
    return true;
  } catch {
    return false;
  }
}

/** Consecutive failed pings since the last success. Module-level rather than
 *  store state — it's an internal debounce counter, not something any
 *  consumer of `useNetworkStore` needs to read. */
let consecutiveServerFailures = 0;

/** Test seam: clears the debounce counter between test cases, since it lives
 *  outside the zustand store (which tests already reset via `setState`). */
export function __resetServerFailureCountForTest(): void {
  consecutiveServerFailures = 0;
}

/** Exported for tests only — production code drives this via the interval
 *  set up in `startNetworkMonitor()`. */
export async function checkServer() {
  const reachable = await pingServer();
  if (reachable) {
    consecutiveServerFailures = 0;
    useNetworkStore.setState({ isServerOnline: true });
    return;
  }
  consecutiveServerFailures += 1;
  if (consecutiveServerFailures >= OFFLINE_FAILURE_THRESHOLD) {
    useNetworkStore.setState({ isServerOnline: false });
  }
}

async function checkInternetStatus() {
  const online = await checkInternet();
  useNetworkStore.setState({ isInternetOnline: online });
}

// ---------------------------------------------------------------------------
// Bootstrap — call once at app startup
// ---------------------------------------------------------------------------

export function startNetworkMonitor() {
  // Immediate checks
  checkServer();
  checkInternetStatus();

  // Periodic server heartbeat
  if (serverIntervalId) clearInterval(serverIntervalId);
  serverIntervalId = setInterval(checkServer, SERVER_PING_INTERVAL);

  // Periodic internet check
  if (internetIntervalId) clearInterval(internetIntervalId);
  internetIntervalId = setInterval(checkInternetStatus, INTERNET_CHECK_INTERVAL);

  // Browser online/offline events as fast hints
  const handleOnline = () => {
    // Trigger an immediate recheck to confirm
    checkInternetStatus();
  };
  const handleOffline = () => {
    useNetworkStore.setState({ isInternetOnline: false });
  };

  window.addEventListener("online", handleOnline);
  window.addEventListener("offline", handleOffline);

  return () => {
    if (serverIntervalId) clearInterval(serverIntervalId);
    serverIntervalId = null;
    if (internetIntervalId) clearInterval(internetIntervalId);
    internetIntervalId = null;
    window.removeEventListener("online", handleOnline);
    window.removeEventListener("offline", handleOffline);
  };
}
