// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

vi.mock("../../../lib/api", () => ({
    getAgentChannels: vi.fn(),
    upsertSlackChannel: vi.fn(),
    setSlackChannelSecret: vi.fn(),
    deleteSlackChannel: vi.fn(),
    getSlackManifest: vi.fn(),
    testSlackConnection: vi.fn(),
    ApiError: class ApiError extends Error {
        status: number;
        constructor(status: number, message: string) {
            super(message);
            this.status = status;
        }
    },
}));

import {
    getAgentChannels,
    upsertSlackChannel,
    setSlackChannelSecret,
    deleteSlackChannel,
    getSlackManifest,
    testSlackConnection,
} from "../../../lib/api";
import { SlackTabPanel, type ChannelSaveHandle } from "../AgentProfileModal";

const getAgentChannelsMock = vi.mocked(getAgentChannels);
const upsertSlackChannelMock = vi.mocked(upsertSlackChannel);
const setSlackChannelSecretMock = vi.mocked(setSlackChannelSecret);
const deleteSlackChannelMock = vi.mocked(deleteSlackChannel);
const getSlackManifestMock = vi.mocked(getSlackManifest);
const testSlackConnectionMock = vi.mocked(testSlackConnection);

const CONFIGURED_STATUS = {
    binding_id: "slack",
    kind: "slack" as const,
    enabled: true,
    bridge_thread_provisioned: true,
    allowed_senders: [],
    secret_stored: true,
    kind_config: {
        allowed_users: ["U12345"],
        allowed_channels: ["C67890"],
        conversation_mode: "per_conversation",
    },
    connection_state: "connected" as const,
};

function setValue(input: HTMLInputElement, value: string) {
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
}

function addListValue(container: HTMLDivElement, id: string, value: string) {
    const input = container.querySelector(`#${id}`) as HTMLInputElement;
    setValue(input, value);
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
}

describe("SlackTabPanel", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
        getAgentChannelsMock.mockReset();
        upsertSlackChannelMock.mockReset();
        setSlackChannelSecretMock.mockReset();
        deleteSlackChannelMock.mockReset();
        getSlackManifestMock.mockReset();
        testSlackConnectionMock.mockReset();
        getSlackManifestMock.mockResolvedValue({ manifest_yaml: "display_information:\n  name: Test Agent\n" });
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("shows a save-first message in create mode without calling the channels endpoint", async () => {
        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "new-agent", isCreating: true }));
        });
        expect(container.textContent).toContain("Save the agent first");
        expect(getAgentChannelsMock).not.toHaveBeenCalled();
        expect(getSlackManifestMock).not.toHaveBeenCalled();
    });

    // This is the regression guard for the silent-failure class this task
    // closes: a Slack binding used to typecheck cleanly (ChannelBinding.kind
    // already listed "slack") but render nothing, because three hardcoded
    // fan-out sites in AgentProfileModal.tsx didn't know about it. Assert on
    // actual rendered output, not on types.
    it("renders an actual Slack panel — not a blank screen — for a Slack binding", async () => {
        getAgentChannelsMock.mockResolvedValue([CONFIGURED_STATUS]);

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });
        await act(async () => { await Promise.resolve(); });

        expect(getAgentChannelsMock).toHaveBeenCalledWith("agent-1");
        expect(container.textContent).toContain("connect this agent to a Slack workspace.");
        expect(container.textContent).toContain("Enabled");
        expect(container.textContent).toContain("Tokens set");
        expect(container.querySelector("#slack-allowed-users")).not.toBeNull();
        expect(container.querySelector("#slack-allowed-channels")).not.toBeNull();
        expect(container.querySelector("#slack-bot-token")).not.toBeNull();
        expect(container.querySelector("#slack-app-token")).not.toBeNull();
        expect(container.textContent).not.toBe("");
    });

    it("renders empty config fields and defaults when no Slack binding exists", async () => {
        getAgentChannelsMock.mockResolvedValue([]);

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Disabled");
        expect(container.textContent).toContain("Tokens not set");
        expect(container.textContent).toContain("not yet provisioned");
    });

    it("seeds fields from an existing status and shows provisioned/enabled/set state", async () => {
        getAgentChannelsMock.mockResolvedValue([CONFIGURED_STATUS]);

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Enabled");
        expect(container.textContent).toContain("Tokens set");
        expect(container.textContent).toContain("provisioned");
        expect(container.textContent).toContain("U12345");
        expect(container.textContent).toContain("C67890");
    });

    it("renders the connected badge", async () => {
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, connection_state: "connected" }]);

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Connected");
    });

    it("renders a non-alarming badge, with an explanatory tooltip, when another process holds the lease", async () => {
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, connection_state: "not-holding-lease" }]);

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Held by another process");
    });

    it("saves configuration with the fields the user entered, including the enable toggle", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertSlackChannelMock.mockResolvedValue({
            ...CONFIGURED_STATUS,
            enabled: true,
            bridge_thread_provisioned: true,
            secret_stored: false,
        });

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            addListValue(container, "slack-allowed-users", "U12345");
            addListValue(container, "slack-allowed-channels", "C67890");
        });

        const enableToggle = Array.from(container.querySelectorAll('button[role="switch"]')).find(
            (b) => b.getAttribute("aria-label") === "Enable Slack channel"
        )! as HTMLButtonElement;
        await act(async () => {
            enableToggle.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });

        expect(ref.current!.isConfigured()).toBe(true);
        await act(async () => {
            await ref.current!.save();
        });

        expect(upsertSlackChannelMock).toHaveBeenCalledWith("agent-1", {
            allowed_users: ["U12345"],
            allowed_channels: ["C67890"],
            conversation_mode: "per_conversation",
            enabled: true,
        });
        expect(container.textContent).toContain("Enabled");
    });

    it("shows the inline error message on a failed config save", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertSlackChannelMock.mockRejectedValue(new Error("allowed_channels must not be blank"));

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        let result: Awaited<ReturnType<ChannelSaveHandle["save"]>> | undefined;
        await act(async () => {
            result = await ref.current!.save();
        });

        expect(result).toEqual({ ok: false, error: "allowed_channels must not be blank" });
        expect(container.textContent).toContain("allowed_channels must not be blank");
    });

    it("saves both tokens in one request, clears the inputs, and never re-renders the raw tokens", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        setSlackChannelSecretMock.mockResolvedValue({ ...CONFIGURED_STATUS, enabled: false, bridge_thread_provisioned: false, secret_stored: true });

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const botTokenInput = container.querySelector("#slack-bot-token") as HTMLInputElement;
        const appTokenInput = container.querySelector("#slack-app-token") as HTMLInputElement;
        await act(async () => {
            setValue(botTokenInput, "xoxb-abc123");
            setValue(appTokenInput, "xapp-def456");
        });

        const saveTokensButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Save tokens")!;
        await act(async () => {
            saveTokensButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(setSlackChannelSecretMock).toHaveBeenCalledWith("agent-1", "xoxb-abc123", "xapp-def456");
        expect(container.textContent).toContain("Tokens set");
        expect(container.textContent).not.toContain("xoxb-abc123");
        expect(container.textContent).not.toContain("xapp-def456");
        expect((container.querySelector("#slack-bot-token") as HTMLInputElement).value).toBe("");
        expect((container.querySelector("#slack-app-token") as HTMLInputElement).value).toBe("");
    });

    it("disables Save tokens until both bot and app tokens are entered", async () => {
        getAgentChannelsMock.mockResolvedValue([]);

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const saveTokensButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Save tokens")! as HTMLButtonElement;
        expect(saveTokensButton.disabled).toBe(true);

        await act(async () => {
            setValue(container.querySelector("#slack-bot-token") as HTMLInputElement, "xoxb-abc123");
        });
        expect(saveTokensButton.disabled).toBe(true);

        await act(async () => {
            setValue(container.querySelector("#slack-app-token") as HTMLInputElement, "xapp-def456");
        });
        expect(saveTokensButton.disabled).toBe(false);
    });

    it("loads and renders the app manifest with a copy-to-clipboard control", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        getSlackManifestMock.mockResolvedValue({ manifest_yaml: "display_information:\n  name: My Agent\n" });
        Object.assign(navigator, { clipboard: { writeText: vi.fn().mockResolvedValue(undefined) } });

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(getSlackManifestMock).toHaveBeenCalledWith("agent-1");
        expect(container.textContent).toContain("display_information:");
        expect(container.textContent).toContain("My Agent");

        const copyButton = container.querySelector('button[aria-label="Copy app manifest"]') as HTMLButtonElement;
        await act(async () => {
            copyButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });

        expect(navigator.clipboard.writeText).toHaveBeenCalledWith("display_information:\n  name: My Agent\n");
    });

    it("runs Test Connection and renders the per-scope green/red list plus workspace name and bot handle", async () => {
        getAgentChannelsMock.mockResolvedValue([CONFIGURED_STATUS]);
        testSlackConnectionMock.mockResolvedValue({
            auth_check: { passed: true, failure: null },
            identity: { team_name: "Acme Corp", team_id: "T0123", bot_handle: "launchpad-bot", bot_user_id: "U0456" },
            scopes: [
                { scope: "chat:write", granted: true },
                { scope: "channels:history", granted: false },
            ],
            connections_open_check: { passed: true, failure: null },
        });

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const testButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes("Test Connection"))! as HTMLButtonElement;
        expect(testButton.disabled).toBe(false);
        await act(async () => {
            testButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(testSlackConnectionMock).toHaveBeenCalledWith("agent-1");
        expect(container.textContent).toContain("Acme Corp");
        expect(container.textContent).toContain("launchpad-bot");
        expect(container.textContent).toContain("chat:write");
        expect(container.textContent).toContain("channels:history");

        const scopeItems = Array.from(container.querySelectorAll("li"));
        const grantedItem = scopeItems.find((li) => li.textContent?.includes("chat:write"))!;
        const missingItem = scopeItems.find((li) => li.textContent?.includes("channels:history"))!;
        expect(grantedItem.className).toContain("text-green-600");
        expect(missingItem.className).not.toContain("text-green-600");
    });

    it("disables Test Connection until both tokens are stored", async () => {
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, secret_stored: false }]);

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const testButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes("Test Connection"))! as HTMLButtonElement;
        expect(testButton.disabled).toBe(true);
        expect(testSlackConnectionMock).not.toHaveBeenCalled();
    });

    it("removes the Slack channel and resets to the not-configured state", async () => {
        getAgentChannelsMock.mockResolvedValue([CONFIGURED_STATUS]);
        deleteSlackChannelMock.mockResolvedValue(undefined);

        await act(async () => {
            root.render(React.createElement(SlackTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const removeButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes("Remove Slack channel"))!;
        await act(async () => {
            removeButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(deleteSlackChannelMock).toHaveBeenCalledWith("agent-1");
        expect(container.textContent).toContain("Disabled");
        expect(container.textContent).toContain("Tokens not set");
    });
});
