// @vitest-environment jsdom
//
// Regression guard for the silent-failure class this task closes: a Slack
// `ChannelBinding` already typechecked cleanly against `ChannelBinding.kind`
// (`types/api.ts`) before this fix — the type union already listed "slack" —
// but three separate hardcoded fan-out sites in `ChannelsTabPanel`
// (`CHANNEL_SUB_TABS`, the `activeChannel` useState union, and the
// conditional render block) had no Slack arm, so clicking a "Slack" sub-tab
// that never existed, or reaching one that rendered nothing, was invisible
// at both compile time and runtime. This test drives the real sub-tab
// switcher end-to-end and asserts the Slack panel actually renders content.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

vi.mock("../../../lib/api", () => ({
    getAgentChannels: vi.fn(),
    getTelegramStatus: vi.fn(),
    getSlackManifest: vi.fn(),
    upsertSlackChannel: vi.fn(),
    setSlackChannelSecret: vi.fn(),
    deleteSlackChannel: vi.fn(),
    testSlackConnection: vi.fn(),
    ApiError: class ApiError extends Error {
        status: number;
        constructor(status: number, message: string) {
            super(message);
            this.status = status;
        }
    },
}));

import { getAgentChannels, getTelegramStatus, getSlackManifest } from "../../../lib/api";
import { ChannelsTabPanel } from "../AgentProfileModal";

const getAgentChannelsMock = vi.mocked(getAgentChannels);
const getTelegramStatusMock = vi.mocked(getTelegramStatus);
const getSlackManifestMock = vi.mocked(getSlackManifest);

describe("ChannelsTabPanel", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
        getAgentChannelsMock.mockReset();
        getTelegramStatusMock.mockReset();
        getSlackManifestMock.mockReset();
        getAgentChannelsMock.mockResolvedValue([]);
        getTelegramStatusMock.mockResolvedValue({ has_token: false, bot_username: null, enabled: false, linked: false });
        getSlackManifestMock.mockResolvedValue({ manifest_yaml: "display_information:\n  name: Test Agent\n" });
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("lists a Slack sub-tab alongside Telegram, Discord, and Email", async () => {
        await act(async () => {
            root.render(React.createElement(ChannelsTabPanel, {
                agentId: "agent-1",
                isCreating: false,
                telegramConfig: null,
                onTelegramConfigChange: () => {},
            }));
        });
        await act(async () => { await Promise.resolve(); });

        const tabLabels = Array.from(container.querySelectorAll("button span")).map((el) => el.textContent);
        expect(tabLabels).toContain("Slack");
    });

    it("renders real Slack panel content — not a blank pane — after clicking the Slack sub-tab", async () => {
        getAgentChannelsMock.mockResolvedValue([{
            binding_id: "slack",
            kind: "slack",
            enabled: true,
            bridge_thread_provisioned: true,
            allowed_senders: [],
            secret_stored: true,
            kind_config: { allowed_users: ["U1"], allowed_channels: ["C1"], conversation_mode: "per_conversation" },
            connection_state: "connected",
        }]);

        await act(async () => {
            root.render(React.createElement(ChannelsTabPanel, {
                agentId: "agent-1",
                isCreating: false,
                telegramConfig: null,
                onTelegramConfigChange: () => {},
            }));
        });
        await act(async () => { await Promise.resolve(); });

        const slackTabButton = Array.from(container.querySelectorAll("button")).find(
            (b) => b.querySelector("span")?.textContent === "Slack",
        )! as HTMLButtonElement;

        await act(async () => {
            slackTabButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });
        await act(async () => { await Promise.resolve(); });

        // The exact failure mode this guards: before the fix, this render
        // produced an empty channels panel (no error, no content) because
        // "slack" matched none of the three hardcoded sites.
        expect(container.textContent).toContain("connect this agent to a Slack workspace.");
        expect(container.querySelector("#slack-allowed-users")).not.toBeNull();
        expect(container.querySelector("#slack-bot-token")).not.toBeNull();
    });
});
