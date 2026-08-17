/**
 * Unit coverage for `isChannelBridgeThread` (lib/threadNavigation.ts), the
 * predicate ChatView uses to decide whether the message composer should be
 * swapped for a read-only hint. It must match the backend's
 * `is_channel_bridge_thread` (`crates/ao-engine/src/agent_runner/native.rs`)
 * exactly: `enabled === true` AND `bridge_thread_id === threadId` for ANY
 * binding in `AgentProfile.channels`.
 *
 * The legacy `AgentProfile.telegram` field this predicate used to read is
 * input-only on the backend (folded into `channels` on deserialize, never
 * re-emitted on output) — a real fetched profile only ever carries
 * `channels`, so these fixtures use that shape.
 */
import { describe, it, expect } from "vitest";
import { isChannelBridgeThread, getBridgeChannelKind } from "../lib/threadNavigation";
import type { AgentProfile, ChannelBinding, ChannelBridgeOrigin } from "../types/api";

const DEFAULT_THREAD_ID = "thread-default-1";
const BRIDGE_THREAD_ID = "thread-bridge-1";
const SLACK_CONVO_THREAD_ID = "thread-slack-convo-1";

function makeBinding(overrides: Partial<ChannelBinding> = {}): ChannelBinding {
  return {
    binding_id: "binding-1",
    kind: "telegram",
    enabled: true,
    bridge_thread_id: BRIDGE_THREAD_ID,
    allowed_senders: [],
    ...overrides,
  };
}

function makeProfile(channels: ChannelBinding[] | undefined): AgentProfile {
  return {
    id: "agent-1",
    name: "Test Agent",
    description: "",
    provider: {
      type: "",
      command: "",
      args: [],
      output_format: "",
      input_mode: "",
      model_aliases: {},
      resume_args: [],
      session_id_fields: [],
      clear_env: false,
      no_output_timeout_ms: 0,
    },
    model: null,
    skills: [],
    system_prompt: null,
    tools: null,
    env: {},
    max_instances: 1,
    timeout_seconds: 0,
    working_dir: null,
    home_dir: null,
    serialize: false,
    channels,
  };
}

describe("isChannelBridgeThread", () => {
  it("is read-only when an enabled Telegram binding's bridge thread is open", () => {
    const profile = makeProfile([makeBinding({ kind: "telegram" })]);
    expect(isChannelBridgeThread(profile, BRIDGE_THREAD_ID)).toBe(true);
  });

  it("is read-only when an enabled Discord binding's bridge thread is open (the bug being fixed)", () => {
    const profile = makeProfile([makeBinding({ kind: "discord", binding_id: "discord-1" })]);
    expect(isChannelBridgeThread(profile, BRIDGE_THREAD_ID)).toBe(true);
  });

  it("is read-only when an enabled Email binding's bridge thread is open", () => {
    const profile = makeProfile([makeBinding({ kind: "email", binding_id: "email-1" })]);
    expect(isChannelBridgeThread(profile, BRIDGE_THREAD_ID)).toBe(true);
  });

  it("stays writable on a normal desktop thread even though the agent has an enabled binding elsewhere (lockout regression guard)", () => {
    const profile = makeProfile([makeBinding({ kind: "discord", binding_id: "discord-1" })]);
    expect(isChannelBridgeThread(profile, DEFAULT_THREAD_ID)).toBe(false);
  });

  it("stays writable when the matching binding is disabled", () => {
    const profile = makeProfile([makeBinding({ enabled: false })]);
    expect(isChannelBridgeThread(profile, BRIDGE_THREAD_ID)).toBe(false);
  });

  it("stays writable for an agent with no channels at all", () => {
    const profile = makeProfile(undefined);
    expect(isChannelBridgeThread(profile, DEFAULT_THREAD_ID)).toBe(false);
  });

  // Slack provisions one thread per *conversation*, not one per binding —
  // its `ChannelBinding.bridge_thread_id` is never populated at runtime, so
  // none of the checks above ever match a real Slack thread. Only the
  // thread's own `channel_origin` (read off the `Thread` row, passed in by
  // the caller) can recognize it. This was the bug reported: Slack threads
  // kept a normal writable composer instead of the read-only hint.
  it("is read-only on a Slack conversation thread via channel_origin, even with no bridge_thread_id anywhere (the bug being fixed)", () => {
    const profile = makeProfile([
      makeBinding({ kind: "slack", binding_id: "slack-1", bridge_thread_id: null }),
    ]);
    const origin: ChannelBridgeOrigin = { kind: "slack", binding_id: "slack-1" };
    expect(isChannelBridgeThread(profile, SLACK_CONVO_THREAD_ID, origin)).toBe(true);
  });

  it("stays writable via channel_origin when the named Slack binding is disabled", () => {
    const profile = makeProfile([
      makeBinding({ kind: "slack", binding_id: "slack-1", enabled: false, bridge_thread_id: null }),
    ]);
    const origin: ChannelBridgeOrigin = { kind: "slack", binding_id: "slack-1" };
    expect(isChannelBridgeThread(profile, SLACK_CONVO_THREAD_ID, origin)).toBe(false);
  });

  it("stays writable via channel_origin when the named binding no longer exists on the profile", () => {
    const profile = makeProfile([]);
    const origin: ChannelBridgeOrigin = { kind: "slack", binding_id: "slack-1" };
    expect(isChannelBridgeThread(profile, SLACK_CONVO_THREAD_ID, origin)).toBe(false);
  });

  it("stays writable when threadChannelOrigin is omitted entirely (back-compat with existing callers)", () => {
    const profile = makeProfile([makeBinding({ kind: "slack", binding_id: "slack-1" })]);
    expect(isChannelBridgeThread(profile, SLACK_CONVO_THREAD_ID)).toBe(false);
  });
});

describe("getBridgeChannelKind", () => {
  it("returns the matched binding's kind so callers can name the actual channel (the bug being fixed — ChatView used to hardcode 'Telegram')", () => {
    const profile = makeProfile([makeBinding({ kind: "discord", binding_id: "discord-1" })]);
    expect(getBridgeChannelKind(profile, BRIDGE_THREAD_ID)).toBe("discord");
  });

  it("returns null on a normal desktop thread even when the agent has an enabled binding elsewhere", () => {
    const profile = makeProfile([makeBinding({ kind: "telegram" })]);
    expect(getBridgeChannelKind(profile, DEFAULT_THREAD_ID)).toBeNull();
  });

  it("returns null when the matching binding is disabled", () => {
    const profile = makeProfile([makeBinding({ enabled: false })]);
    expect(getBridgeChannelKind(profile, BRIDGE_THREAD_ID)).toBeNull();
  });

  it("agrees with isChannelBridgeThread's boolean for every fixture above", () => {
    const profile = makeProfile([makeBinding({ kind: "email", binding_id: "email-1" })]);
    expect(getBridgeChannelKind(profile, BRIDGE_THREAD_ID) !== null).toBe(
      isChannelBridgeThread(profile, BRIDGE_THREAD_ID),
    );
  });

  it("returns 'slack' via channel_origin for a per-conversation Slack thread with no bridge_thread_id", () => {
    const profile = makeProfile([
      makeBinding({ kind: "slack", binding_id: "slack-1", bridge_thread_id: null }),
    ]);
    const origin: ChannelBridgeOrigin = { kind: "slack", binding_id: "slack-1" };
    expect(getBridgeChannelKind(profile, SLACK_CONVO_THREAD_ID, origin)).toBe("slack");
  });

  it("returns null via channel_origin when the named Slack binding is disabled", () => {
    const profile = makeProfile([
      makeBinding({ kind: "slack", binding_id: "slack-1", enabled: false, bridge_thread_id: null }),
    ]);
    const origin: ChannelBridgeOrigin = { kind: "slack", binding_id: "slack-1" };
    expect(getBridgeChannelKind(profile, SLACK_CONVO_THREAD_ID, origin)).toBeNull();
  });
});
