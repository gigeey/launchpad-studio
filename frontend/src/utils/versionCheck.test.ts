// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  calculateVersionDistance,
  isVersionTooOld,
  MAX_VERSION_DISTANCE,
  fetchLatestVersion,
} from "./versionCheck";

describe("calculateVersionDistance", () => {
  it("returns 0 for the same version", () => {
    expect(calculateVersionDistance("0.12.0", "0.12.0")).toBe(0);
  });

  it("ignores patch versions", () => {
    expect(calculateVersionDistance("0.12.0", "0.12.9")).toBe(0);
  });

  it("counts minor version bumps", () => {
    expect(calculateVersionDistance("0.12.0", "0.14.0")).toBe(2);
  });

  it("counts major version bumps heavily", () => {
    expect(calculateVersionDistance("0.12.0", "1.12.0")).toBe(1000);
  });

  it("handles mixed major and minor differences", () => {
    expect(calculateVersionDistance("0.12.0", "1.14.0")).toBe(1002);
  });

  it("handles v-prefix", () => {
    expect(calculateVersionDistance("v0.12.0", "v0.14.0")).toBe(2);
  });
});

describe("isVersionTooOld", () => {
  it("returns false when exactly MAX_VERSION_DISTANCE behind", () => {
    expect(isVersionTooOld("0.4.0", "0.14.0")).toBe(false);
  });

  it("returns true when 11 behind", () => {
    expect(isVersionTooOld("0.3.0", "0.14.0")).toBe(true);
  });

  it("returns false when versions are the same", () => {
    expect(isVersionTooOld("0.14.0", "0.14.0")).toBe(false);
  });

  it("returns false when only patch versions differ", () => {
    expect(isVersionTooOld("0.14.0", "0.14.9")).toBe(false);
  });
});

describe("MAX_VERSION_DISTANCE", () => {
  it("is 10", () => {
    expect(MAX_VERSION_DISTANCE).toBe(10);
  });
});

// ---------------------------------------------------------------------------
// fetchLatestVersion
// ---------------------------------------------------------------------------

describe("fetchLatestVersion", () => {
  const CACHE_KEY = "launchpad_latest_version";

  beforeEach(() => {
    localStorage.clear();
    vi.restoreAllMocks();
  });

  it("returns the version from a successful fetch and caches it", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify({ version: "0.20.0" }), { status: 200 })
    );

    const version = await fetchLatestVersion();
    expect(version).toBe("0.20.0");

    const cached = JSON.parse(localStorage.getItem(CACHE_KEY)!);
    expect(cached.version).toBe("0.20.0");
    expect(cached.lastChecked).toBeTruthy();
  });

  it("falls back to cached version when fetch fails", async () => {
    localStorage.setItem(
      CACHE_KEY,
      JSON.stringify({ version: "0.18.0", lastChecked: "2026-01-01T00:00:00.000Z" })
    );
    vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("network error"));

    const version = await fetchLatestVersion();
    expect(version).toBe("0.18.0");
  });

  it("returns null when fetch fails and no cache exists (fail open)", async () => {
    vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("network error"));

    const version = await fetchLatestVersion();
    expect(version).toBeNull();
  });

  it("falls back to cache when response is not ok", async () => {
    localStorage.setItem(
      CACHE_KEY,
      JSON.stringify({ version: "0.17.0", lastChecked: "2026-01-01T00:00:00.000Z" })
    );
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("Not Found", { status: 404 })
    );

    const version = await fetchLatestVersion();
    expect(version).toBe("0.17.0");
  });

  it("falls back to cache when response has no version field", async () => {
    localStorage.setItem(
      CACHE_KEY,
      JSON.stringify({ version: "0.16.0", lastChecked: "2026-01-01T00:00:00.000Z" })
    );
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify({}), { status: 200 })
    );

    const version = await fetchLatestVersion();
    expect(version).toBe("0.16.0");
  });
});
