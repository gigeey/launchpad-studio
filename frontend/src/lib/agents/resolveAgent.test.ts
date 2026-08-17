import { describe, it, expect } from "vitest";
import {
  resolveAgent,
  truncateId,
  FALLBACK_EMOJI,
} from "./resolveAgent";
import type { AgentSnapshot } from "../../types/api";

function makeSnapshot(partial: Partial<AgentSnapshot> & { agent_id: string }): AgentSnapshot {
  return {
    agent_id: partial.agent_id,
    name: partial.name ?? "Unnamed",
    emoji: partial.emoji,
    last_activity_at: partial.last_activity_at ?? null,
    last_agent_activity_at: partial.last_agent_activity_at,
    last_message: partial.last_message,
    message_count: partial.message_count ?? 0,
    has_active_run: partial.has_active_run ?? false,
    queue_depth: partial.queue_depth ?? 0,
    thread_id: partial.thread_id ?? null,
    created_at: partial.created_at ?? "2026-01-01T00:00:00Z",
    file_capabilities_supported: partial.file_capabilities_supported,
  };
}

describe("truncateId", () => {
  it("returns full id when length <= 8", () => {
    expect(truncateId("abc")).toBe("abc");
    expect(truncateId("abcdefgh")).toBe("abcdefgh");
  });

  it("truncates long ids to 8 chars + ellipsis", () => {
    expect(truncateId("abcdefghi")).toBe("abcdefgh…");
    expect(truncateId("0123456789abcdef")).toBe("01234567…");
  });

  it("returns empty string unchanged", () => {
    expect(truncateId("")).toBe("");
  });
});

describe("resolveAgent", () => {
  const snapshots: AgentSnapshot[] = [
    makeSnapshot({ agent_id: "alpha-123", name: "Alpha Agent", emoji: "🚀" }),
    makeSnapshot({ agent_id: "beta", name: "Beta", emoji: "🎯" }),
    makeSnapshot({ agent_id: "no-emoji", name: "Emoji-less" }),
  ];

  it("returns the snapshot values when id is found", () => {
    const resolved = resolveAgent("alpha-123", snapshots);
    expect(resolved).toEqual({
      id: "alpha-123",
      displayName: "Alpha Agent",
      emoji: "🚀",
      isFallback: false,
    });
  });

  it("falls back to the robot emoji when the snapshot has no emoji", () => {
    const resolved = resolveAgent("no-emoji", snapshots);
    expect(resolved.isFallback).toBe(false);
    expect(resolved.emoji).toBe(FALLBACK_EMOJI);
    expect(resolved.displayName).toBe("Emoji-less");
  });

  it("returns a fallback when the id is missing from snapshots", () => {
    const resolved = resolveAgent("ghost-id-xyz", snapshots);
    expect(resolved).toEqual({
      id: "ghost-id-xyz",
      displayName: "ghost-id…",
      emoji: FALLBACK_EMOJI,
      isFallback: true,
    });
  });

  it("returns a fallback for an empty id", () => {
    const resolved = resolveAgent("", snapshots);
    expect(resolved).toEqual({
      id: "",
      displayName: "",
      emoji: FALLBACK_EMOJI,
      isFallback: true,
    });
  });

  it("does not truncate short unknown ids", () => {
    const resolved = resolveAgent("short", snapshots);
    expect(resolved).toEqual({
      id: "short",
      displayName: "short",
      emoji: FALLBACK_EMOJI,
      isFallback: true,
    });
  });
});
