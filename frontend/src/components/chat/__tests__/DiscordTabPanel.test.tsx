// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

vi.mock("../../../lib/api", () => ({
    getAgentChannels: vi.fn(),
    upsertDiscordChannel: vi.fn(),
    setDiscordChannelSecret: vi.fn(),
    deleteDiscordChannel: vi.fn(),
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
    upsertDiscordChannel,
    setDiscordChannelSecret,
    deleteDiscordChannel,
} from "../../../lib/api";
import { DiscordTabPanel, type ChannelSaveHandle } from "../AgentProfileModal";

const getAgentChannelsMock = vi.mocked(getAgentChannels);
const upsertDiscordChannelMock = vi.mocked(upsertDiscordChannel);
const setDiscordChannelSecretMock = vi.mocked(setDiscordChannelSecret);
const deleteDiscordChannelMock = vi.mocked(deleteDiscordChannel);

const CONFIGURED_STATUS = {
    binding_id: "discord",
    kind: "discord" as const,
    enabled: true,
    bridge_thread_provisioned: true,
    allowed_senders: [],
    secret_stored: true,
    kind_config: {
        allowed_users: ["12345"],
        allowed_roles: ["67890"],
        allowed_channels: ["11111"],
        dm_role_auth_guild: "22222",
        require_mention: true,
        thread_follow: "sticky_decay",
        thread_idle_timeout_minutes: 15,
        thread_message_budget: 10,
        backfill_limit: 20,
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

describe("DiscordTabPanel", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
        getAgentChannelsMock.mockReset();
        upsertDiscordChannelMock.mockReset();
        setDiscordChannelSecretMock.mockReset();
        deleteDiscordChannelMock.mockReset();
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("shows a save-first message in create mode without calling the channels endpoint", async () => {
        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "new-agent", isCreating: true }));
        });
        expect(container.textContent).toContain("Save the agent first");
        expect(getAgentChannelsMock).not.toHaveBeenCalled();
    });

    it("renders empty config fields and defaults when no Discord binding exists", async () => {
        getAgentChannelsMock.mockResolvedValue([]);

        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(getAgentChannelsMock).toHaveBeenCalledWith("agent-1");
        const guild = container.querySelector("#discord-dm-role-auth-guild") as HTMLInputElement;
        expect(guild.value).toBe("");
        expect(container.textContent).toContain("Disabled");
        expect(container.textContent).toContain("Bot token not set");
        expect(container.textContent).toContain("not yet provisioned");

        const mentionToggle = Array.from(container.querySelectorAll('button[role="switch"]')).find(
            (b) => b.getAttribute("aria-label") === "Respond to every message, not just mentions",
        )! as HTMLButtonElement;
        expect(mentionToggle.getAttribute("aria-checked")).toBe("true");
        expect((container.querySelector("#discord-thread-follow") as HTMLSelectElement).value).toBe("sticky_decay");
        expect((container.querySelector("#discord-thread-idle-timeout") as HTMLInputElement).value).toBe("15");
        expect((container.querySelector("#discord-thread-message-budget") as HTMLInputElement).value).toBe("10");
        expect((container.querySelector("#discord-backfill-limit") as HTMLInputElement).value).toBe("20");
    });

    it("seeds fields from an existing status and shows provisioned/enabled/set state", async () => {
        getAgentChannelsMock.mockResolvedValue([{
            ...CONFIGURED_STATUS,
            kind_config: {
                ...CONFIGURED_STATUS.kind_config,
                require_mention: false,
                thread_follow: "always",
                thread_idle_timeout_minutes: 45,
                thread_message_budget: 30,
                backfill_limit: 5,
            },
        }]);

        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const guild = container.querySelector("#discord-dm-role-auth-guild") as HTMLInputElement;
        expect(guild.value).toBe("22222");
        expect(container.textContent).toContain("Enabled");
        expect(container.textContent).toContain("Bot token set");
        expect(container.textContent).toContain("provisioned");
        expect(container.textContent).toContain("12345");
        expect(container.textContent).toContain("67890");
        expect(container.textContent).toContain("11111");

        const mentionToggle = Array.from(container.querySelectorAll('button[role="switch"]')).find(
            (b) => b.getAttribute("aria-label") === "Only respond when mentioned",
        )! as HTMLButtonElement;
        expect(mentionToggle.getAttribute("aria-checked")).toBe("false");
        expect((container.querySelector("#discord-thread-follow") as HTMLSelectElement).value).toBe("always");
        // "always" mode hides the idle-timeout/message-budget fields, which
        // only apply to sticky-decay.
        expect(container.querySelector("#discord-thread-idle-timeout")).toBeNull();
        expect(container.querySelector("#discord-thread-message-budget")).toBeNull();
        expect((container.querySelector("#discord-backfill-limit") as HTMLInputElement).value).toBe("5");
    });

    // --- Per-binding connection state ---
    //
    // Deliberately distinct from the "Enabled"/"Disabled" badge tested above,
    // which only reflects saved config — an enabled binding can still read
    // as disconnected, reconnecting, or held by another backend process.

    it("renders the connected badge", async () => {
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, connection_state: "connected" }]);

        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Connected");
    });

    it("renders the reconnecting badge", async () => {
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, connection_state: "reconnecting" }]);

        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Reconnecting");
    });

    it("renders the disconnected badge", async () => {
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, connection_state: "disconnected" }]);

        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Disconnected");
    });

    it("renders a non-alarming badge, with an explanatory tooltip, when another process holds the lease", async () => {
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, connection_state: "not-holding-lease" }]);

        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Held by another process");
        const badge = Array.from(container.querySelectorAll("span")).find((el) =>
            el.textContent?.includes("Held by another process")
        );
        expect(badge?.getAttribute("title")).toMatch(/another backend process/i);
        expect(badge?.getAttribute("title")).toMatch(/isn't an error/i);
    });

    it("saves configuration with the fields the user entered, including the enable toggle", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertDiscordChannelMock.mockResolvedValue({
            ...CONFIGURED_STATUS,
            enabled: true,
            bridge_thread_provisioned: true,
            secret_stored: false,
        });

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            addListValue(container, "discord-allowed-users", "12345");
            addListValue(container, "discord-allowed-roles", "67890");
            addListValue(container, "discord-allowed-channels", "11111");
            setValue(container.querySelector("#discord-dm-role-auth-guild") as HTMLInputElement, "22222");
        });

        const enableToggle = Array.from(container.querySelectorAll('button[role="switch"]')).find(
            (b) => b.getAttribute("aria-label") === "Enable Discord channel"
        )! as HTMLButtonElement;
        await act(async () => {
            enableToggle.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });

        expect(ref.current!.isConfigured()).toBe(true);
        await act(async () => {
            await ref.current!.save();
        });

        expect(upsertDiscordChannelMock).toHaveBeenCalledWith("agent-1", {
            allowed_users: ["12345"],
            allowed_roles: ["67890"],
            allowed_channels: ["11111"],
            dm_role_auth_guild: "22222",
            require_mention: true,
            thread_follow: "sticky_decay",
            thread_idle_timeout_minutes: 15,
            thread_message_budget: 10,
            backfill_limit: 20,
            enabled: true,
        });
        expect(container.textContent).toContain("Enabled");
    });

    it("saves the engagement fields the user changed: mention toggle, follow mode, idle timeout, message budget, backfill limit", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertDiscordChannelMock.mockResolvedValue({ ...CONFIGURED_STATUS, enabled: false });

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const mentionToggle = Array.from(container.querySelectorAll('button[role="switch"]')).find(
            (b) => b.getAttribute("aria-label") === "Respond to every message, not just mentions",
        )! as HTMLButtonElement;
        await act(async () => {
            mentionToggle.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });

        const followSelect = container.querySelector("#discord-thread-follow") as HTMLSelectElement;
        await act(async () => {
            const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, "value")!.set!;
            setter.call(followSelect, "one_shot");
            followSelect.dispatchEvent(new Event("change", { bubbles: true }));
        });

        const backfillInput = container.querySelector("#discord-backfill-limit") as HTMLInputElement;
        await act(async () => {
            setValue(backfillInput, "0");
        });

        await act(async () => {
            await ref.current!.save();
        });

        expect(upsertDiscordChannelMock).toHaveBeenCalledWith("agent-1", expect.objectContaining({
            require_mention: false,
            thread_follow: "one_shot",
            backfill_limit: 0,
        }));
    });

    it("hides the idle-timeout/message-budget fields once one-shot or always is selected", async () => {
        getAgentChannelsMock.mockResolvedValue([CONFIGURED_STATUS]);

        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.querySelector("#discord-thread-idle-timeout")).not.toBeNull();

        const followSelect = container.querySelector("#discord-thread-follow") as HTMLSelectElement;
        await act(async () => {
            const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, "value")!.set!;
            setter.call(followSelect, "always");
            followSelect.dispatchEvent(new Event("change", { bubbles: true }));
        });

        expect(container.querySelector("#discord-thread-idle-timeout")).toBeNull();
        expect(container.querySelector("#discord-thread-message-budget")).toBeNull();
    });

    it("sends null dm_role_auth_guild when the field is left blank", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertDiscordChannelMock.mockResolvedValue({ ...CONFIGURED_STATUS, kind_config: { ...CONFIGURED_STATUS.kind_config, dm_role_auth_guild: null } });

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            await ref.current!.save();
        });

        expect(upsertDiscordChannelMock).toHaveBeenCalledWith("agent-1", expect.objectContaining({
            dm_role_auth_guild: null,
        }));
    });

    it("shows the inline error message on a failed config save", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertDiscordChannelMock.mockRejectedValue(new Error("dm_role_auth_guild must not be blank"));

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        let result: Awaited<ReturnType<ChannelSaveHandle["save"]>> | undefined;
        await act(async () => {
            result = await ref.current!.save();
        });

        expect(result).toEqual({ ok: false, error: "dm_role_auth_guild must not be blank" });
        expect(container.textContent).toContain("dm_role_auth_guild must not be blank");
    });

    it("sets the bot token, clears the input, and never re-renders the raw token", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        setDiscordChannelSecretMock.mockResolvedValue({ ...CONFIGURED_STATUS, enabled: false, bridge_thread_provisioned: false, secret_stored: true });

        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const tokenInput = container.querySelector("#discord-bot-token") as HTMLInputElement;
        await act(async () => {
            setValue(tokenInput, "abc123.def456.ghi789");
        });

        const setTokenButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Set bot token")!;
        await act(async () => {
            setTokenButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(setDiscordChannelSecretMock).toHaveBeenCalledWith("agent-1", "abc123.def456.ghi789");
        expect(container.textContent).toContain("Bot token set");
        expect(container.textContent).not.toContain("abc123.def456.ghi789");
        expect((container.querySelector("#discord-bot-token") as HTMLInputElement).value).toBe("");
    });

    it("removes the Discord channel and resets to the not-configured state", async () => {
        getAgentChannelsMock.mockResolvedValue([CONFIGURED_STATUS]);
        deleteDiscordChannelMock.mockResolvedValue(undefined);

        await act(async () => {
            root.render(React.createElement(DiscordTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const removeButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes("Remove Discord channel"))!;
        await act(async () => {
            removeButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(deleteDiscordChannelMock).toHaveBeenCalledWith("agent-1");
        expect(container.textContent).toContain("Disabled");
        expect(container.textContent).toContain("Bot token not set");
        expect((container.querySelector("#discord-dm-role-auth-guild") as HTMLInputElement).value).toBe("");
    });
});
