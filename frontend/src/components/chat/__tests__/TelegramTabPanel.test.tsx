// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

vi.mock("../../../lib/api", () => ({
    getTelegramStatus: vi.fn(),
    getAgentChannels: vi.fn(),
    setTelegramToken: vi.fn(),
    deleteTelegramToken: vi.fn(),
    createTelegramPairingCode: vi.fn(),
    unlinkTelegramChat: vi.fn(),
    ApiError: class ApiError extends Error {
        status: number;
        constructor(status: number, message: string) {
            super(message);
            this.status = status;
        }
    },
}));

import {
    getTelegramStatus,
    getAgentChannels,
    setTelegramToken,
    deleteTelegramToken,
    createTelegramPairingCode,
    unlinkTelegramChat,
} from "../../../lib/api";
import { TelegramTabPanel } from "../AgentProfileModal";
import type { TelegramConfig } from "../../../types/api";

const getTelegramStatusMock = vi.mocked(getTelegramStatus);
const getAgentChannelsMock = vi.mocked(getAgentChannels);
const setTelegramTokenMock = vi.mocked(setTelegramToken);
const deleteTelegramTokenMock = vi.mocked(deleteTelegramToken);
const createTelegramPairingCodeMock = vi.mocked(createTelegramPairingCode);
const unlinkTelegramChatMock = vi.mocked(unlinkTelegramChat);

/** Full `GET …/channels` row for the Telegram binding, with `connection_state`
 *  set to whatever this test wants to prove renders correctly. */
function telegramChannelRow(connectionState: import("../../../lib/api").ChannelConnectionState) {
    return {
        binding_id: "telegram",
        kind: "telegram" as const,
        enabled: true,
        bridge_thread_provisioned: true,
        allowed_senders: [],
        secret_stored: true,
        kind_config: {},
        connection_state: connectionState,
    };
}

/** Mirrors how AgentProfileFormBody lifts the config draft, so the toggle test
 *  below exercises the same onConfigChange contract the real form relies on. */
function ConfigDraftHost({ onDraft }: { onDraft: (draft: TelegramConfig | null) => void }) {
    const [config, setConfig] = React.useState<TelegramConfig | null>(null);
    React.useEffect(() => { onDraft(config); }, [config, onDraft]);
    return React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false, config, onConfigChange: setConfig });
}

describe("TelegramTabPanel", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
        getTelegramStatusMock.mockReset();
        getAgentChannelsMock.mockReset();
        getAgentChannelsMock.mockResolvedValue([]);
        setTelegramTokenMock.mockReset();
        deleteTelegramTokenMock.mockReset();
        createTelegramPairingCodeMock.mockReset();
        unlinkTelegramChatMock.mockReset();
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("shows a save-first message in create mode without calling the status endpoint", async () => {
        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "new-agent", isCreating: true }));
        });
        expect(container.textContent).toContain("Save the agent first");
        expect(getTelegramStatusMock).not.toHaveBeenCalled();
    });

    it("renders the token input when not configured", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: false, bot_username: null, enabled: false, linked: false });

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(getTelegramStatusMock).toHaveBeenCalledWith("agent-1");
        const input = container.querySelector("#tg-token") as HTMLInputElement | null;
        expect(input).not.toBeNull();
        expect(container.textContent).not.toContain("Connected");
    });

    it("renders @bot_username and the Enabled label when configured", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false });

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("@axew_research_bot");
        expect(container.textContent).toContain("Enabled");
        expect(container.querySelector("#tg-token")).toBeNull();
    });

    it("keeps the How to connect help collapsed by default and expands it on click", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false });

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const toggle = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "How to connect")!;
        expect(toggle).not.toBeUndefined();
        expect(toggle.getAttribute("aria-expanded")).toBe("false");
        expect(container.textContent).not.toContain("single-use and expires in 10 minutes");

        await act(async () => {
            toggle.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });

        expect(toggle.getAttribute("aria-expanded")).toBe("true");
        expect(container.textContent).toContain("single-use and expires in 10 minutes");
        expect(container.textContent).toContain("Group Privacy");
    });

    // --- Per-binding connection state ---
    //
    // Deliberately distinct from the "Enabled"/"Disabled" label above, which
    // only reflects the saved config: an enabled binding can still be
    // disconnected, reconnecting, or held by another backend process.

    it("renders the connected badge when the transport reports a live session", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false });
        getAgentChannelsMock.mockResolvedValue([telegramChannelRow("connected")]);

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Connected");
    });

    it("renders the reconnecting badge while the poll loop is backing off", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false });
        getAgentChannelsMock.mockResolvedValue([telegramChannelRow("reconnecting")]);

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Reconnecting");
    });

    it("renders the disconnected badge when no process is running the binding", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false });
        getAgentChannelsMock.mockResolvedValue([telegramChannelRow("disconnected")]);

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Disconnected");
    });

    it("renders a non-alarming badge, with an explanatory tooltip, when another process holds the lease", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false });
        getAgentChannelsMock.mockResolvedValue([telegramChannelRow("not-holding-lease")]);

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Held by another process");
        const badge = Array.from(container.querySelectorAll("span")).find((el) =>
            el.textContent?.includes("Held by another process")
        );
        expect(badge?.getAttribute("title")).toMatch(/another backend process/i);
        expect(badge?.getAttribute("title")).toMatch(/isn't an error/i);
    });

    it("saves a token, clears the input, and never re-renders the raw token", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: false, bot_username: null, enabled: false, linked: false });
        setTelegramTokenMock.mockResolvedValue({ bot_username: "axew_research_bot" });

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const input = container.querySelector("#tg-token") as HTMLInputElement;
        await act(async () => {
            input.dispatchEvent(new Event("focusin", { bubbles: true }));
            const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
            setter.call(input, "123456:SECRET-TOKEN");
            input.dispatchEvent(new Event("input", { bubbles: true }));
        });

        const saveButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Set Token")!;
        await act(async () => {
            saveButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(setTelegramTokenMock).toHaveBeenCalledWith("agent-1", "123456:SECRET-TOKEN");
        expect(container.textContent).toContain("@axew_research_bot");
        expect(container.textContent).not.toContain("123456:SECRET-TOKEN");
        expect(container.querySelector("#tg-token")).toBeNull();
    });

    it("shows the inline error message on an invalid-token save failure", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: false, bot_username: null, enabled: false, linked: false });
        setTelegramTokenMock.mockRejectedValue(new Error("invalid Telegram bot token"));

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const input = container.querySelector("#tg-token") as HTMLInputElement;
        await act(async () => {
            const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
            setter.call(input, "bogus");
            input.dispatchEvent(new Event("input", { bubbles: true }));
        });

        const saveButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Set Token")!;
        await act(async () => {
            saveButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("invalid Telegram bot token");
        // Still not configured — the input remains for another attempt.
        expect(container.querySelector("#tg-token")).not.toBeNull();
    });

    it("disconnects and returns to the not-configured state", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false });
        deleteTelegramTokenMock.mockResolvedValue(undefined);

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const disconnectButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes("Disconnect"))!;
        await act(async () => {
            disconnectButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(deleteTelegramTokenMock).toHaveBeenCalledWith("agent-1");
        expect(container.textContent).not.toContain("@axew_research_bot");
        expect(container.querySelector("#tg-token")).not.toBeNull();
    });

    it("syncs the config draft from status and lets the enable toggle stage a change without touching the token", async () => {
        getTelegramStatusMock.mockResolvedValue({ has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false });

        let latestDraft: TelegramConfig | null = null;
        await act(async () => {
            root.render(React.createElement(ConfigDraftHost, { onDraft: (d) => { latestDraft = d; } }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(latestDraft).toEqual({
            enabled: true,
            bot_username: "axew_research_bot",
            thread_mode: "dedicated",
            bridge_thread_id: null,
            allowed_chat_ids: [],
        });

        const toggle = container.querySelector('button[role="switch"]') as HTMLButtonElement;
        expect(toggle.getAttribute("aria-checked")).toBe("true");
        await act(async () => {
            toggle.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });

        expect(latestDraft).toMatchObject({ enabled: false, bot_username: "axew_research_bot" });
        expect(setTelegramTokenMock).not.toHaveBeenCalled();
        expect(deleteTelegramTokenMock).not.toHaveBeenCalled();
    });

    it("generates a pairing code and renders it with the /start instruction", async () => {
        getTelegramStatusMock
            .mockResolvedValueOnce({
                has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false,
                allowed_chat_ids: [], pending_pairing_code: null,
            })
            .mockResolvedValueOnce({
                has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false,
                allowed_chat_ids: [], pending_pairing_code: { code: "ABC123", expires_at_unix: Math.floor(Date.now() / 1000) + 600 },
            });
        createTelegramPairingCodeMock.mockResolvedValue({ code: "ABC123", expires_at_unix: Math.floor(Date.now() / 1000) + 600 });

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const generateButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Generate pairing code")!;
        await act(async () => {
            generateButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(createTelegramPairingCodeMock).toHaveBeenCalledWith("agent-1");
        expect(getTelegramStatusMock).toHaveBeenCalledTimes(2);
        expect(container.textContent).toContain("ABC123");
        expect(container.textContent).toContain("send /start ABC123 to your bot to link this chat.");
    });

    it("shows an already-pending unexpired pairing code on load without forcing a regenerate", async () => {
        getTelegramStatusMock.mockResolvedValue({
            has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false,
            allowed_chat_ids: [], pending_pairing_code: { code: "XYZ999", expires_at_unix: Math.floor(Date.now() / 1000) + 300 },
        });

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("XYZ999");
        expect(createTelegramPairingCodeMock).not.toHaveBeenCalled();
        const generateButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes("Generate"))!;
        expect(generateButton.textContent).toBe("Generate new code");
    });

    it("renders linked chat ids and unlinks one, updating the list from the response", async () => {
        getTelegramStatusMock.mockResolvedValue({
            has_token: true, bot_username: "axew_research_bot", enabled: true, linked: true,
            allowed_chat_ids: [111, 222], pending_pairing_code: null,
        });
        unlinkTelegramChatMock.mockResolvedValue({ allowed_chat_ids: [222] });

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("111");
        expect(container.textContent).toContain("222");

        const unlinkButtons = Array.from(container.querySelectorAll("button")).filter((b) => b.textContent === "Unlink");
        expect(unlinkButtons).toHaveLength(2);
        await act(async () => {
            unlinkButtons[0].dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(unlinkTelegramChatMock).toHaveBeenCalledWith("agent-1", 111);
        expect(container.textContent).not.toContain("111");
        expect(container.textContent).toContain("222");
    });

    it("shows the no-chats-linked hint and reject-all note when the allow-list is empty", async () => {
        getTelegramStatusMock.mockResolvedValue({
            has_token: true, bot_username: "axew_research_bot", enabled: true, linked: false,
            allowed_chat_ids: [], pending_pairing_code: null,
        });

        await act(async () => {
            root.render(React.createElement(TelegramTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("No chats linked yet");
        expect(container.textContent).toContain("the bot ignores all incoming messages");
    });
});
