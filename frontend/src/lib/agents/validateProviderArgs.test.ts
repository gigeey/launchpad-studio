import { describe, it, expect } from "vitest";
import { validateProviderArgs } from "./validateProviderArgs";
import { AGENT_TEMPLATES } from "../../data/agentTemplates";

describe("validateProviderArgs — shipped default templates", () => {
    it("claude template validates clean", () => {
        expect(validateProviderArgs(AGENT_TEMPLATES.claude.provider.command, AGENT_TEMPLATES.claude.provider.args)).toEqual([]);
    });

    it("cursor template validates clean", () => {
        expect(validateProviderArgs(AGENT_TEMPLATES.cursor.provider.command, AGENT_TEMPLATES.cursor.provider.args)).toEqual([]);
    });

    it("codex template validates clean", () => {
        expect(validateProviderArgs(AGENT_TEMPLATES.codex.provider.command, AGENT_TEMPLATES.codex.provider.args)).toEqual([]);
    });

    it("agy template validates clean", () => {
        expect(validateProviderArgs(AGENT_TEMPLATES.agy.provider.command, AGENT_TEMPLATES.agy.provider.args)).toEqual([]);
    });
});

describe("validateProviderArgs — cross-contamination detection", () => {
    it("flags a Claude-only flag pasted into a codex profile", () => {
        const warnings = validateProviderArgs("codex", ["exec", "--json", "--sandbox", "workspace-write", "--skip-git-repo-check", "--dangerously-skip-permissions"]);
        expect(warnings).toHaveLength(1);
        expect(warnings[0]).toContain("--dangerously-skip-permissions");
        expect(warnings[0]).toContain("Claude");
        expect(warnings[0]).toContain("codex");
    });

    it("flags --mcp-config pasted into a codex profile (codex has no such flag)", () => {
        const warnings = validateProviderArgs("codex", ["exec", "--json", "--sandbox", "workspace-write", "--skip-git-repo-check", "--mcp-config", "/tmp/x.json"]);
        expect(warnings).toHaveLength(1);
        expect(warnings[0]).toContain("--mcp-config");
    });

    it("does not flag --mcp-config for claude or cursor-agent (both accept it)", () => {
        expect(validateProviderArgs("claude", ["--print", "--mcp-config", "/tmp/x.json"])).toEqual([]);
        expect(validateProviderArgs("cursor-agent", ["--print", "--mcp-config", "/tmp/x.json"])).toEqual([]);
    });

    it("flags a cursor-only flag pasted into a claude profile", () => {
        const warnings = validateProviderArgs("claude", ["--print", "--approve-mcps"]);
        expect(warnings).toHaveLength(1);
        expect(warnings[0]).toContain("--approve-mcps");
        expect(warnings[0]).toContain("cursor-agent");
    });

    it("flags a codex-only flag pasted into a cursor-agent profile and names the silent-exit failure mode", () => {
        const warnings = validateProviderArgs("cursor-agent", ["--print", "--output-format", "stream-json", "--skip-git-repo-check"]);
        expect(warnings).toHaveLength(1);
        expect(warnings[0]).toContain("--skip-git-repo-check");
        expect(warnings[0]).toContain("silently exit 1");
    });

    it("resolves the command through a path, matching by basename", () => {
        const warnings = validateProviderArgs("/usr/local/bin/codex", ["exec", "--dangerously-skip-permissions"]);
        expect(warnings).toHaveLength(1);
    });

    it("does not flag --model pasted into an agy profile (agy genuinely uses --model)", () => {
        const warnings = validateProviderArgs("agy", ["--dangerously-skip-permissions", "--model", "gemini"]);
        expect(warnings).toEqual([]);
    });

    it("flags an agy-only flag (--conversation) pasted into a claude profile", () => {
        const warnings = validateProviderArgs("claude", ["--print", "--conversation", "abc123"]);
        expect(warnings).toHaveLength(1);
        expect(warnings[0]).toContain("--conversation");
        expect(warnings[0]).toContain("agy");
    });
});

describe("validateProviderArgs — shared flags never flagged", () => {
    it("never flags --print, --output-format, or --model regardless of provider", () => {
        expect(validateProviderArgs("codex", ["--print", "--output-format", "stream-json", "--model", "gpt-5"])).toEqual([]);
        expect(validateProviderArgs("claude", ["--print", "--output-format", "stream-json", "--model", "opus"])).toEqual([]);
        expect(validateProviderArgs("cursor-agent", ["--print", "--output-format", "stream-json", "--model", "auto"])).toEqual([]);
    });
});

describe("validateProviderArgs — unknown commands and flags", () => {
    it("returns no warnings for a command that isn't one of the three known providers", () => {
        expect(validateProviderArgs("echo", ["Hello from agent"])).toEqual([]);
        expect(validateProviderArgs("my-custom-cli", ["--some-novel-flag"])).toEqual([]);
    });

    it("does not flag novel/unrecognized flags on a known provider", () => {
        expect(validateProviderArgs("claude", ["--print", "--some-brand-new-claude-flag"])).toEqual([]);
    });
});
