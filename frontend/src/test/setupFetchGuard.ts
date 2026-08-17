import { beforeEach, expect, vi } from "vitest";

/**
 * `src/lib/api.ts` falls back to `http://localhost:3001` whenever
 * `import.meta.env.VITE_API_BASE_URL` isn't set — and it never is under
 * vitest, because that value only exists in this repo via direnv's
 * `.envrc` (a shell-only mechanism vitest's process never sources; Vite
 * only merges *already-present* `VITE_`-prefixed `process.env` vars, it
 * doesn't invoke direnv itself). So any test that exercises a real
 * `api.ts` call path without mocking `fetch` doesn't hit a configured
 * endpoint — it dials a real socket on port 3001 that nothing in this
 * worktree is listening on (this worktree is pinned to 3101). That
 * produces nondeterministic `ECONNREFUSED` unhandled rejections and
 * 5000ms request timeouts instead of a fast, readable failure, and lets a
 * run "pass" with the wrong exit code.
 *
 * This resets `globalThis.fetch` before every test to a stub that throws
 * immediately, naming the method, URL, and originating test so the
 * failure points straight at the missing mock. A test that legitimately
 * wants network behavior must opt in explicitly by assigning its own
 * `globalThis.fetch = vi.fn(...)` (see `AsyncFormGating.test.ts` for the
 * established pattern) — resetting here on every `beforeEach` means a
 * test that forgets to restore its own override in `afterEach` can't leak
 * a stale mock into the next test either.
 */
beforeEach(() => {
  const testPath = expect.getState().testPath ?? "<unknown test file>";
  const testName = expect.getState().currentTestName ?? "<unknown test>";
  globalThis.fetch = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    const url = input instanceof Request ? input.url : String(input);
    throw new Error(
      `Unmocked fetch: ${method} ${url}\n` +
        `  from test: "${testName}" (${testPath})\n` +
        `  Tests must not perform real network calls — mock globalThis.fetch explicitly.`,
    );
  }) as typeof fetch;
});
