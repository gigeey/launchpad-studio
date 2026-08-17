// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

vi.mock("../../../lib/api", () => ({
    getAgentChannels: vi.fn(),
    getChannelSenders: vi.fn(),
    setChannelSenders: vi.fn(),
    upsertEmailChannel: vi.fn(),
    setEmailChannelSecret: vi.fn(),
    deleteEmailChannel: vi.fn(),
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
    getChannelSenders,
    setChannelSenders,
    upsertEmailChannel,
    setEmailChannelSecret,
    deleteEmailChannel,
} from "../../../lib/api";
import { EmailTabPanel, type ChannelSaveHandle } from "../AgentProfileModal";

const getAgentChannelsMock = vi.mocked(getAgentChannels);
const getChannelSendersMock = vi.mocked(getChannelSenders);
const setChannelSendersMock = vi.mocked(setChannelSenders);
const upsertEmailChannelMock = vi.mocked(upsertEmailChannel);
const setEmailChannelSecretMock = vi.mocked(setEmailChannelSecret);
const deleteEmailChannelMock = vi.mocked(deleteEmailChannel);

const CONFIGURED_STATUS = {
    binding_id: "email",
    kind: "email" as const,
    enabled: true,
    bridge_thread_provisioned: true,
    allowed_senders: ["boss@example.com"],
    secret_stored: true,
    kind_config: {
        address: "agent@example.com",
        imap_host: "imap.example.com",
        imap_port: 993,
        smtp_host: "smtp.example.com",
        smtp_port: 587,
        poll_secs: 120,
        require_auth_results: true,
    },
    connection_state: "connected" as const,
};

// Mirrors a real backend: the returned ChannelStatus reflects whatever config
// was actually persisted by the most recent upsertEmailChannel call, so tests
// can prove the reconcile step never shows stale/clobbered field values.
function echoStatus(config: Record<string, unknown>, secretStored: boolean) {
    return {
        binding_id: "email",
        kind: "email" as const,
        enabled: config.enabled as boolean,
        bridge_thread_provisioned: config.enabled as boolean,
        allowed_senders: config.allowed_senders as string[],
        secret_stored: secretStored,
        kind_config: {
            address: config.address,
            imap_host: config.imap_host,
            imap_port: config.imap_port,
            smtp_host: config.smtp_host,
            smtp_port: config.smtp_port,
            poll_secs: config.poll_secs,
            require_auth_results: config.require_auth_results,
        },
        connection_state: "connected" as const,
    };
}

function setValue(input: HTMLInputElement, value: string) {
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
}

function pressEnter(input: HTMLInputElement) {
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
}

function findButton(container: HTMLElement, text: string) {
    return Array.from(container.querySelectorAll("button")).find((b) => b.textContent === text) as HTMLButtonElement;
}

describe("EmailTabPanel", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
        getAgentChannelsMock.mockReset();
        getChannelSendersMock.mockReset();
        setChannelSendersMock.mockReset();
        upsertEmailChannelMock.mockReset();
        setEmailChannelSecretMock.mockReset();
        deleteEmailChannelMock.mockReset();
        // Sane defaults so tests that don't care about the allow-list don't
        // need to stub these — overridden per-test where the list matters.
        getChannelSendersMock.mockResolvedValue({ senders: [] });
        setChannelSendersMock.mockImplementation(async (_agentId, _bindingId, senders) => ({ senders }));
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("shows a save-first message in create mode without calling the channels endpoint", async () => {
        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { agentId: "new-agent", isCreating: true }));
        });
        expect(container.textContent).toContain("Save the agent first");
        expect(getAgentChannelsMock).not.toHaveBeenCalled();
    });

    it("renders empty config fields and defaults when no Email binding exists", async () => {
        getAgentChannelsMock.mockResolvedValue([]);

        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(getAgentChannelsMock).toHaveBeenCalledWith("agent-1");
        const address = container.querySelector("#email-address") as HTMLInputElement;
        expect(address.value).toBe("");
        const imapPort = container.querySelector("#email-imap-port") as HTMLInputElement;
        expect(imapPort.value).toBe("993");
        const smtpPort = container.querySelector("#email-smtp-port") as HTMLInputElement;
        expect(smtpPort.value).toBe("587");
        const pollSecs = container.querySelector("#email-poll-secs") as HTMLInputElement;
        expect(pollSecs.value).toBe("300");
        expect(container.textContent).toContain("Disabled");
        expect(container.textContent).toContain("Password not set");
        expect(container.textContent).toContain("not yet provisioned");

        const passwordInput = container.querySelector("#email-password") as HTMLInputElement;
        expect(passwordInput.placeholder).toBe("App password");
        // No per-tab save button — this tab's config/senders/password all
        // save through the imperative ChannelSaveHandle the modal's single
        // primary Save button drives (see the ref-based tests below).
        expect(findButton(container, "Connect")).toBeUndefined();
        expect(findButton(container, "Update Connection")).toBeUndefined();
        expect(findButton(container, "Save")).toBeUndefined();
        expect(findButton(container, "Save configuration")).toBeUndefined();
        expect(findButton(container, "Set password")).toBeUndefined();
    });

    it("seeds fields from an existing status and shows provisioned/enabled/set state", async () => {
        getAgentChannelsMock.mockResolvedValue([CONFIGURED_STATUS]);
        getChannelSendersMock.mockResolvedValue({ senders: ["boss@example.com"] });

        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(getChannelSendersMock).toHaveBeenCalledWith("agent-1", "email");
        const address = container.querySelector("#email-address") as HTMLInputElement;
        expect(address.value).toBe("agent@example.com");
        const imapHost = container.querySelector("#email-imap-host") as HTMLInputElement;
        expect(imapHost.value).toBe("imap.example.com");
        const pollSecs = container.querySelector("#email-poll-secs") as HTMLInputElement;
        expect(pollSecs.value).toBe("120");
        expect(container.textContent).toContain("Enabled");
        expect(container.textContent).toContain("Password set");
        expect(container.textContent).toContain("provisioned");
        expect(container.textContent).toContain("boss@example.com");

        const passwordInput = container.querySelector("#email-password") as HTMLInputElement;
        expect(passwordInput.placeholder).toBe("Leave blank to keep current password");
        expect(passwordInput.value).toBe("");
    });

    it("reads the allow-list from the dedicated senders endpoint, not the (stale) generic channel status", async () => {
        // The generic ChannelStatus.allowed_senders mirrors the deprecated
        // inline profile copy and can disagree with the real, current
        // allow-list — this proves the panel trusts the dedicated GET, not
        // the status field, when the two diverge.
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, allowed_senders: ["stale@example.com"] }]);
        getChannelSendersMock.mockResolvedValue({ senders: ["current@example.com"] });

        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("current@example.com");
        expect(container.textContent).not.toContain("stale@example.com");
    });

    // --- Per-binding connection state ---

    it("renders the not-holding-lease badge with a non-alarming, explanatory tooltip", async () => {
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, connection_state: "not-holding-lease" as const }]);

        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Held by another process");
        const badge = Array.from(container.querySelectorAll("span")).find((el) =>
            el.textContent?.includes("Held by another process")
        );
        expect(badge?.getAttribute("title")).toMatch(/another backend process/i);
    });

    it("renders the disconnected badge when no binding is running", async () => {
        getAgentChannelsMock.mockResolvedValue([{ ...CONFIGURED_STATUS, connection_state: "disconnected" as const }]);

        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Disconnected");
    });

    it("(a) calling save() fires the config PUT with the full draft, including allowed_senders and the enable toggle", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertEmailChannelMock.mockResolvedValue({
            ...CONFIGURED_STATUS,
            enabled: true,
            bridge_thread_provisioned: true,
            secret_stored: false,
        });

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            setValue(container.querySelector("#email-address") as HTMLInputElement, "agent@example.com");
            setValue(container.querySelector("#email-imap-host") as HTMLInputElement, "imap.example.com");
            setValue(container.querySelector("#email-smtp-host") as HTMLInputElement, "smtp.example.com");
        });

        await act(async () => {
            const sendersInput = container.querySelector("#email-allowed-senders") as HTMLInputElement;
            setValue(sendersInput, "boss@example.com");
            pressEnter(sendersInput);
        });

        const enableToggle = Array.from(container.querySelectorAll('button[role="switch"]')).find(
            (b) => b.getAttribute("aria-label") === "Enable Email channel"
        )! as HTMLButtonElement;
        await act(async () => {
            enableToggle.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });

        expect(ref.current!.isConfigured()).toBe(true);
        await act(async () => {
            await ref.current!.save();
        });

        expect(upsertEmailChannelMock).toHaveBeenCalledWith("agent-1", expect.objectContaining({
            address: "agent@example.com",
            imap_host: "imap.example.com",
            imap_port: 993,
            smtp_host: "smtp.example.com",
            smtp_port: 587,
            poll_secs: 300,
            require_auth_results: true,
            allowed_senders: ["boss@example.com"],
            enabled: true,
        }));
        expect(setChannelSendersMock).toHaveBeenCalledWith("agent-1", "email", ["boss@example.com"]);
        expect(setEmailChannelSecretMock).not.toHaveBeenCalled();
        expect(container.textContent).toContain("Enabled");
        expect(container.textContent).toContain("boss@example.com");
    });

    it("keeps the just-saved allow-list visible even when the config PUT echoes a stale allowed_senders (the bug being fixed)", async () => {
        // Mirrors the real backend: upsertEmailChannel's response carries the
        // deprecated inline allowed_senders (here: stale/empty), while the
        // dedicated senders PUT reports what was actually persisted. Save
        // must trust the latter, or a user's edit would appear to silently
        // revert immediately after Save.
        getAgentChannelsMock.mockResolvedValue([]);
        upsertEmailChannelMock.mockResolvedValue({ ...CONFIGURED_STATUS, allowed_senders: [], secret_stored: false });
        setChannelSendersMock.mockResolvedValue({ senders: ["boss@example.com"] });

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            const sendersInput = container.querySelector("#email-allowed-senders") as HTMLInputElement;
            setValue(sendersInput, "boss@example.com");
            pressEnter(sendersInput);
        });

        await act(async () => {
            await ref.current!.save();
        });

        expect(container.textContent).toContain("boss@example.com");
    });

    it("(c) save() with a blank password does not fire the secret PUT", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertEmailChannelMock.mockResolvedValue({ ...CONFIGURED_STATUS, secret_stored: false });

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            await ref.current!.save();
        });

        expect(upsertEmailChannelMock).toHaveBeenCalled();
        expect(setEmailChannelSecretMock).not.toHaveBeenCalled();
    });

    it("(b, e) save() with a non-blank password fires the secret PUT only after the config PUT, then clears the password field", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        let lastConfig: Record<string, unknown> | undefined;
        upsertEmailChannelMock.mockImplementation(async (_id, config) => {
            lastConfig = config as unknown as Record<string, unknown>;
            return echoStatus(lastConfig, false);
        });
        setEmailChannelSecretMock.mockImplementation(async () => echoStatus(lastConfig!, true));

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            setValue(container.querySelector("#email-address") as HTMLInputElement, "agent@example.com");
            setValue(container.querySelector("#email-password") as HTMLInputElement, "hunter2-app-password");
        });

        await act(async () => {
            await ref.current!.save();
        });

        expect(upsertEmailChannelMock).toHaveBeenCalledWith("agent-1", expect.objectContaining({ address: "agent@example.com" }));
        expect(setEmailChannelSecretMock).toHaveBeenCalledWith("agent-1", "hunter2-app-password");
        expect(upsertEmailChannelMock.mock.invocationCallOrder[0]).toBeLessThan(
            setEmailChannelSecretMock.mock.invocationCallOrder[0]
        );
        expect(container.textContent).toContain("Password set");
        expect(container.textContent).not.toContain("hunter2-app-password");
        expect((container.querySelector("#email-password") as HTMLInputElement).value).toBe("");
    });

    it("(d) typing into config fields then saving (with a password) does not lose the typed values", async () => {
        getAgentChannelsMock.mockResolvedValue([CONFIGURED_STATUS]);
        let lastConfig: Record<string, unknown> | undefined;
        upsertEmailChannelMock.mockImplementation(async (_id, config) => {
            lastConfig = config as unknown as Record<string, unknown>;
            return echoStatus(lastConfig, false);
        });
        // If handleSave ever fired the secret PUT before the config PUT, lastConfig
        // would still be undefined here and this would throw, failing the test.
        setEmailChannelSecretMock.mockImplementation(async () => echoStatus(lastConfig!, true));

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect((container.querySelector("#email-address") as HTMLInputElement).value).toBe("agent@example.com");

        await act(async () => {
            setValue(container.querySelector("#email-address") as HTMLInputElement, "fresh@example.com");
            setValue(container.querySelector("#email-imap-host") as HTMLInputElement, "fresh-imap.example.com");
            setValue(container.querySelector("#email-password") as HTMLInputElement, "new-app-password");
        });

        await act(async () => {
            await ref.current!.save();
        });

        expect((container.querySelector("#email-address") as HTMLInputElement).value).toBe("fresh@example.com");
        expect((container.querySelector("#email-imap-host") as HTMLInputElement).value).toBe("fresh-imap.example.com");
        expect((container.querySelector("#email-password") as HTMLInputElement).value).toBe("");
    });

    it("does not fire the secret PUT when the config PUT fails, even with a password entered", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertEmailChannelMock.mockRejectedValue(new Error("a valid email address is required"));

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            setValue(container.querySelector("#email-password") as HTMLInputElement, "hunter2-app-password");
        });

        let result: Awaited<ReturnType<ChannelSaveHandle["save"]>> | undefined;
        await act(async () => {
            result = await ref.current!.save();
        });

        expect(result).toEqual({ ok: false, error: "a valid email address is required" });
        expect(upsertEmailChannelMock).toHaveBeenCalled();
        expect(setEmailChannelSecretMock).not.toHaveBeenCalled();
        expect(container.textContent).toContain("a valid email address is required");
    });

    it("surfaces a clear error noting the config was saved when the secret PUT fails after the config PUT succeeds", async () => {
        getAgentChannelsMock.mockResolvedValue([]);
        upsertEmailChannelMock.mockResolvedValue({ ...CONFIGURED_STATUS, secret_stored: false });
        setEmailChannelSecretMock.mockRejectedValue(new Error("secret store unavailable"));

        const ref = React.createRef<ChannelSaveHandle>();
        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { ref, agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            setValue(container.querySelector("#email-password") as HTMLInputElement, "hunter2-app-password");
        });

        let result: Awaited<ReturnType<ChannelSaveHandle["save"]>> | undefined;
        await act(async () => {
            result = await ref.current!.save();
        });

        expect(result?.ok).toBe(false);
        expect(upsertEmailChannelMock).toHaveBeenCalled();
        expect(setEmailChannelSecretMock).toHaveBeenCalled();
        expect(container.textContent).toContain("Configuration was saved");
        expect(container.textContent).toContain("secret store unavailable");
    });

    it("removes the Email channel and resets to the not-configured state", async () => {
        getAgentChannelsMock.mockResolvedValue([CONFIGURED_STATUS]);
        deleteEmailChannelMock.mockResolvedValue(undefined);

        await act(async () => {
            root.render(React.createElement(EmailTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        const removeButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes("Remove Email channel"))!;
        await act(async () => {
            removeButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(deleteEmailChannelMock).toHaveBeenCalledWith("agent-1");
        expect(container.textContent).toContain("Disabled");
        expect(container.textContent).toContain("Password not set");
        expect((container.querySelector("#email-address") as HTMLInputElement).value).toBe("");
    });
});
