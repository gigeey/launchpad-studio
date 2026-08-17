// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

// Mock Tauri APIs not available in jsdom — `useCliDetection` resolves
// CLI binaries via @tauri-apps/api/core::invoke.
vi.mock("@tauri-apps/api/core", () => ({
    invoke: vi.fn().mockResolvedValue(false),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
    open: vi.fn().mockResolvedValue(null),
}));
// Mirrors the real `ProviderModelDiscoveryError` shape from ../../../lib/api
// (message + optional `code`) — the component does `err instanceof
// ProviderModelDiscoveryError`, so the mocked module needs a real class,
// not a plain object, for that check to hold. Declared via vi.hoisted()
// since vi.mock's factory is hoisted above normal top-level declarations.
const { MockProviderModelDiscoveryError } = vi.hoisted(() => {
    class MockProviderModelDiscoveryError extends Error {
        code?: string;
        constructor(message: string, code?: string) {
            super(message);
            this.name = "ProviderModelDiscoveryError";
            this.code = code;
        }
    }
    return { MockProviderModelDiscoveryError };
});

vi.mock("../../../lib/api", () => ({
    getWorkflows: vi.fn().mockResolvedValue([]),
    getProviderStatuses: vi.fn().mockResolvedValue([]),
    setProviderApiKey: vi.fn().mockResolvedValue(undefined),
    deleteProviderApiKey: vi.fn().mockResolvedValue(undefined),
    getProviderModels: vi.fn().mockResolvedValue([]),
    ProviderModelDiscoveryError: MockProviderModelDiscoveryError,
}));

import {
    CoordinatorConfigFields,
    DEFAULT_COORDINATOR_CONFIG_VALUE,
    coordinatorConfigFromProfile,
    maxTurnsValidationError,
    parseMaxTurns,
    providerSupportsModelDiscovery,
    providerSupportsReasoningEffort,
    type CoordinatorConfigFieldsValue,
} from "../CoordinatorConfigFields";
import { getProviderModels, getProviderStatuses, setProviderApiKey, ProviderModelDiscoveryError } from "../../../lib/api";
import type { AgentNativeProvider, AgentProfile } from "../../../types/api";

function makeAgentProfile(overrides: Partial<AgentProfile> = {}): AgentProfile {
    return {
        id: "test-agent",
        name: "Test Agent",
        description: "Test",
        provider: {
            type: "Cli",
            command: "echo",
            args: [],
            output_format: "Text",
            input_mode: "Arg",
            model_aliases: {},
            resume_args: [],
            session_id_fields: [],
            clear_env: false,
            no_output_timeout_ms: 30000,
        },
        model: null,
        skills: [],
        system_prompt: null,
        tools: null,
        env: {},
        max_instances: 1,
        timeout_seconds: 300,
        working_dir: null,
        home_dir: null,
        serialize: true,
        ...overrides,
    };
}

describe("CoordinatorConfigFields runnerMode plumbing", () => {
    it("DEFAULT_COORDINATOR_CONFIG_VALUE.runnerMode defaults to 'cli'", () => {
        expect(DEFAULT_COORDINATOR_CONFIG_VALUE.runnerMode).toBe("cli");
    });

    it("coordinatorConfigFromProfile() falls back to 'cli' when runner_mode is absent", () => {
        const value = coordinatorConfigFromProfile(makeAgentProfile());
        expect(value.runnerMode).toBe("cli");
    });

    it("coordinatorConfigFromProfile() reads runner_mode='api' from a profile", () => {
        const value = coordinatorConfigFromProfile(makeAgentProfile({ runner_mode: "api" }));
        expect(value.runnerMode).toBe("api");
    });
});

describe("CoordinatorConfigFields kind dropdown render", () => {
    let container: HTMLDivElement;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
    });

    afterEach(async () => {
        await act(async () => {
            root.unmount();
        });
        document.body.removeChild(container);
    });

    function render(value: CoordinatorConfigFieldsValue, opts: { lockRunnerMode?: boolean } = {}) {
        return act(async () => {
            root.render(
                React.createElement(CoordinatorConfigFields, {
                    value,
                    onChange: () => {},
                    lockRunnerMode: opts.lockRunnerMode,
                })
            );
        });
    }

    it("renders the dropdown with both options on create path", async () => {
        await render(DEFAULT_COORDINATOR_CONFIG_VALUE);

        const select = container.querySelector("#ae-kind") as HTMLSelectElement;
        expect(select).toBeTruthy();
        expect(select.disabled).toBe(false);
        expect(select.value).toBe("cli");

        const optionLabels = Array.from(select.options).map((o) => o.text);
        expect(optionLabels).toContain("CLI");
        expect(optionLabels).toContain("Native (API)");
    });

    it("renders disabled and reflects 'api' when locked on edit path", async () => {
        await render(coordinatorConfigFromProfile(makeAgentProfile({ runner_mode: "api" })), { lockRunnerMode: true });

        const select = container.querySelector("#ae-kind") as HTMLSelectElement;
        expect(select).toBeTruthy();
        expect(select.disabled).toBe(true);
        expect(select.value).toBe("api");
    });

    it("respects custom idPrefix so multiple instances don't collide", async () => {
        await act(async () => {
            root.render(
                React.createElement(CoordinatorConfigFields, {
                    value: DEFAULT_COORDINATOR_CONFIG_VALUE,
                    onChange: () => {},
                    idPrefix: "team-coord-",
                })
            );
        });

        expect(container.querySelector("#team-coord-kind")).toBeTruthy();
        expect(container.querySelector("#ae-kind")).toBeNull();
    });
});

describe("CoordinatorConfigFields API-mode provider config (combobox, custom model ID, gating)", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
        vi.mocked(getProviderModels).mockReset().mockResolvedValue([]);
        vi.mocked(setProviderApiKey).mockReset().mockResolvedValue(undefined);
    });

    afterEach(async () => {
        await act(async () => {
            root.unmount();
        });
        document.body.removeChild(container);
    });

    function apiModeValue(nativeProvider: AgentNativeProvider): CoordinatorConfigFieldsValue {
        return { ...DEFAULT_COORDINATOR_CONFIG_VALUE, runnerMode: "api", nativeProvider };
    }

    function render(value: CoordinatorConfigFieldsValue) {
        return act(async () => {
            root.render(React.createElement(CoordinatorConfigFields, { value, onChange: () => {} }));
        });
    }

    function saveButton(): HTMLButtonElement {
        const button = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim().startsWith("Save"));
        if (!button) throw new Error("Save button not found");
        return button;
    }

    // Bypasses React's controlled-input value tracker so the synthetic
    // onChange actually fires (same trick used elsewhere in this suite,
    // e.g. HomeSidebar.test.tsx's setInputValue).
    function setInputValue(input: HTMLInputElement, value: string) {
        const nativeSetter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
        nativeSetter.call(input, value);
        input.dispatchEvent(new Event("input", { bubbles: true }));
    }

    it("includes openrouter in the API-provider options", async () => {
        await render(apiModeValue("anthropic"));
        const select = container.querySelector("#ae-native-provider") as HTMLSelectElement;
        const optionValues = Array.from(select.options).map((o) => o.value);
        expect(optionValues).toEqual(["anthropic", "openai", "openrouter"]);
    });

    it("hides the model/endpoint controls for a provider outside the discovery-capable set, but keeps the key field", async () => {
        // Casts past the AgentNativeProvider union on purpose — this proves
        // the gate itself (providerSupportsModelDiscovery), not just that
        // today's three real options all happen to be capable.
        const nonCapable = "gemini" as unknown as AgentNativeProvider;
        expect(providerSupportsModelDiscovery(nonCapable)).toBe(false);

        await render(apiModeValue(nonCapable));

        expect(container.querySelector("#ae-provider-api-key")).toBeTruthy();
        expect(container.querySelector("#ae-provider-base-url")).toBeNull();
        expect(container.querySelector("#ae-provider-model-select")).toBeNull();
        expect(container.querySelector("#ae-provider-model-custom")).toBeNull();
    });

    it("shows the model combobox and endpoint field for a discovery-capable provider", async () => {
        await render(apiModeValue("openrouter"));
        await act(async () => {}); // flush the mount-triggered discovery call

        expect(container.querySelector("#ae-provider-base-url")).toBeTruthy();
        expect(container.querySelector("#ae-provider-model-select")).toBeTruthy();
        expect(container.querySelector("#ae-provider-model-custom")).toBeTruthy();
    });

    it("keeps a typed custom model ID after discovery fails — the free-text field is never gated on discovery succeeding", async () => {
        vi.mocked(getProviderModels).mockRejectedValue(new ProviderModelDiscoveryError("upstream unreachable", "network_failure"));

        await render(apiModeValue("openai"));
        await act(async () => {}); // flush the mount-triggered (failing) discovery call

        const customInput = container.querySelector("#ae-provider-model-custom") as HTMLInputElement;
        expect(customInput).toBeTruthy();
        expect(customInput.disabled).toBe(false);

        await act(async () => {
            setInputValue(customInput, "ft:gpt-4o-mini:my-org::custom123");
        });

        expect((container.querySelector("#ae-provider-model-custom") as HTMLInputElement).value).toBe(
            "ft:gpt-4o-mini:my-org::custom123",
        );
        // The dropdown stays present and merely unselected — discovery
        // failing never hides or disables the custom field next to it.
        const select = container.querySelector("#ae-provider-model-select") as HTMLSelectElement;
        expect(select).toBeTruthy();
        expect(select.value).toBe("");
    });

    it("persists the key via debounced implicit validation even when the provider rejects it — a soft warning, not a save-blocker", async () => {
        vi.mocked(getProviderModels).mockRejectedValue(new ProviderModelDiscoveryError("invalid api key", "auth_failure"));

        await render(apiModeValue("anthropic"));
        await act(async () => {}); // flush the mount-triggered discovery call (also rejects, harmless)

        const keyInput = container.querySelector("#ae-provider-api-key") as HTMLInputElement;

        vi.useFakeTimers();
        try {
            await act(async () => {
                setInputValue(keyInput, "sk-bad-key");
            });
            await act(async () => {
                await vi.advanceTimersByTimeAsync(300);
            });
        } finally {
            vi.useRealTimers();
        }

        // The debounced flow saved the key regardless of what discovery
        // later reported — no "Test Connection" gate in front of it.
        expect(setProviderApiKey).toHaveBeenCalledWith("anthropic", "sk-bad-key", expect.any(Object));

        // Soft, non-blocking warning.
        expect(container.textContent).toContain("rejected this key");

        // Nothing about the auth failure disables the explicit Save path.
        expect(saveButton().disabled).toBe(false);
    });

    it("sends model and base_url alongside api_key on explicit Save", async () => {
        await render(apiModeValue("openai"));
        await act(async () => {}); // flush mount-triggered discovery

        const keyInput = container.querySelector("#ae-provider-api-key") as HTMLInputElement;
        const baseUrlInput = container.querySelector("#ae-provider-base-url") as HTMLInputElement;
        const modelInput = container.querySelector("#ae-provider-model-custom") as HTMLInputElement;

        // Fake timers so the debounced implicit-validation `setTimeout`
        // (triggered by the keyInput edit below) stays pending rather than
        // firing on real wall-clock time and racing this test's own
        // explicit-Save assertion with a second `setProviderApiKey` call.
        vi.useFakeTimers();
        try {
            await act(async () => {
                setInputValue(keyInput, "sk-live-key");
                setInputValue(baseUrlInput, "http://localhost:11434/v1");
                setInputValue(modelInput, "llama3");
            });

            await act(async () => {
                saveButton().dispatchEvent(new MouseEvent("click", { bubbles: true }));
            });

            expect(setProviderApiKey).toHaveBeenCalledWith("openai", "sk-live-key", {
                baseUrl: "http://localhost:11434/v1",
                model: "llama3",
            });
        } finally {
            vi.useRealTimers();
        }
    });

    it("prefills max output tokens, max context tokens, and reasoning effort from GET /providers", async () => {
        vi.mocked(getProviderStatuses).mockResolvedValue([
            {
                provider: "anthropic",
                has_api_key: true,
                base_url: null,
                model: null,
                max_output_tokens: 4096,
                max_context_tokens: 80000,
                reasoning_effort: "medium",
            },
        ]);

        await render(apiModeValue("anthropic"));
        await act(async () => {}); // flush the mount-triggered refresh + discovery calls

        expect((container.querySelector("#ae-provider-max-output-tokens") as HTMLInputElement).value).toBe("4096");
        expect((container.querySelector("#ae-provider-max-context-tokens") as HTMLInputElement).value).toBe("80000");
        expect((container.querySelector("#ae-provider-reasoning-effort") as HTMLSelectElement).value).toBe("medium");
    });

    it("sends max output tokens, max context tokens, and reasoning effort alongside api_key on explicit Save", async () => {
        await render(apiModeValue("anthropic"));
        await act(async () => {}); // flush mount-triggered discovery

        const keyInput = container.querySelector("#ae-provider-api-key") as HTMLInputElement;
        const maxOutputInput = container.querySelector("#ae-provider-max-output-tokens") as HTMLInputElement;
        const maxContextInput = container.querySelector("#ae-provider-max-context-tokens") as HTMLInputElement;
        const reasoningSelect = container.querySelector("#ae-provider-reasoning-effort") as HTMLSelectElement;

        vi.useFakeTimers();
        try {
            await act(async () => {
                setInputValue(keyInput, "sk-live-key");
                setInputValue(maxOutputInput, "4096");
                setInputValue(maxContextInput, "80000");
            });
            await act(async () => {
                reasoningSelect.value = "high";
                reasoningSelect.dispatchEvent(new Event("change", { bubbles: true }));
            });

            await act(async () => {
                saveButton().dispatchEvent(new MouseEvent("click", { bubbles: true }));
            });

            expect(setProviderApiKey).toHaveBeenCalledWith(
                "anthropic",
                "sk-live-key",
                expect.objectContaining({
                    maxOutputTokens: 4096,
                    maxContextTokens: 80000,
                    reasoningEffort: "high",
                }),
            );
        } finally {
            vi.useRealTimers();
        }
    });

    it("hides the reasoning-effort control for a provider outside the reasoning-capable set, but keeps max output/context tokens", async () => {
        // Casts past the AgentNativeProvider union on purpose — proves the
        // gate itself (providerSupportsReasoningEffort), not just that
        // today's three real options all happen to be capable.
        const nonCapable = "gemini" as unknown as AgentNativeProvider;
        expect(providerSupportsReasoningEffort(nonCapable)).toBe(false);

        await render(apiModeValue(nonCapable));

        expect(container.querySelector("#ae-provider-reasoning-effort")).toBeNull();
        // Universally-supported knobs aren't gated by this predicate at all.
        expect(container.querySelector("#ae-provider-max-output-tokens")).toBeTruthy();
        expect(container.querySelector("#ae-provider-max-context-tokens")).toBeTruthy();
    });

    it("shows the key fingerprint in the API key placeholder when GET /providers reports one", async () => {
        vi.mocked(getProviderStatuses).mockResolvedValue([
            {
                provider: "anthropic",
                has_api_key: true,
                api_key_fingerprint: "sk-ant-api03…7f2a",
                base_url: null,
                model: null,
                max_output_tokens: null,
                max_context_tokens: null,
                reasoning_effort: null,
            },
        ]);

        await render(apiModeValue("anthropic"));
        await act(async () => {}); // flush the mount-triggered status refresh + discovery calls

        const keyInput = container.querySelector("#ae-provider-api-key") as HTMLInputElement;
        expect(keyInput.placeholder).toContain("sk-ant-api03…7f2a");
        expect(keyInput.placeholder).not.toContain("••••••••");
    });

    it("falls back to the dot placeholder when a key is configured but no fingerprint is reported", async () => {
        vi.mocked(getProviderStatuses).mockResolvedValue([
            {
                provider: "anthropic",
                has_api_key: true,
                api_key_fingerprint: null,
                base_url: null,
                model: null,
                max_output_tokens: null,
                max_context_tokens: null,
                reasoning_effort: null,
            },
        ]);

        await render(apiModeValue("anthropic"));
        await act(async () => {});

        const keyInput = container.querySelector("#ae-provider-api-key") as HTMLInputElement;
        expect(keyInput.placeholder).toContain("••••••••");
    });

    it("includes the fingerprint and replace instruction in the 401 warning when GET /providers reports one", async () => {
        vi.mocked(getProviderStatuses).mockResolvedValue([
            {
                provider: "anthropic",
                has_api_key: true,
                api_key_fingerprint: "sk-ant-api03…7f2a",
                base_url: null,
                model: null,
                max_output_tokens: null,
                max_context_tokens: null,
                reasoning_effort: null,
            },
        ]);
        vi.mocked(getProviderModels).mockRejectedValue(new ProviderModelDiscoveryError("invalid api key", "auth_failure"));

        await render(apiModeValue("anthropic"));
        await act(async () => {}); // flush the mount-triggered status refresh + (failing) discovery call

        expect(container.textContent).toContain("sk-ant-api03…7f2a");
        expect(container.textContent).toContain("Enter a new key to replace it");
        expect(container.textContent).not.toContain("rejected this key");
    });

    it("shows a neutral invitation — not an error — and never calls discovery when no key is stored", async () => {
        vi.mocked(getProviderStatuses).mockResolvedValue([
            {
                provider: "anthropic",
                has_api_key: false,
                base_url: null,
                model: null,
                max_output_tokens: null,
                max_context_tokens: null,
                reasoning_effort: null,
            },
        ]);

        await render(apiModeValue("anthropic"));
        await act(async () => {}); // flush the mount-triggered status refresh

        expect(container.textContent).toContain(
            "No API key configured — paste one to enable Anthropic (Claude) models.",
        );

        // Discovery (GET /providers/{name}/models) is the only source of an
        // HTTP status code in this panel — proving it was never called
        // proves no status code could have been rendered, not just that
        // none happens to be visible right now.
        expect(getProviderModels).not.toHaveBeenCalled();
        expect(container.textContent).not.toContain("Couldn't load the model list");
        expect(container.textContent).not.toContain("rejected");

        const invitation = Array.from(container.querySelectorAll("p")).find((p) =>
            p.textContent?.startsWith("No API key configured"),
        );
        expect(invitation).toBeTruthy();
        // Neutral styling: no warning icon and no amber/error color class —
        // the two visual cues the rejected-key and other-failure states use.
        expect(invitation!.querySelector("svg")).toBeNull();
        expect(invitation!.className).not.toMatch(/amber|error/);
    });

    it("still renders the 401 rejected-key warning, distinct from the no-key invitation", async () => {
        vi.mocked(getProviderStatuses).mockResolvedValue([
            {
                provider: "anthropic",
                has_api_key: true,
                api_key_fingerprint: "sk-ant-api03…7f2a",
                base_url: null,
                model: null,
                max_output_tokens: null,
                max_context_tokens: null,
                reasoning_effort: null,
            },
        ]);
        vi.mocked(getProviderModels).mockRejectedValue(new ProviderModelDiscoveryError("invalid api key", "auth_failure"));

        await render(apiModeValue("anthropic"));
        await act(async () => {}); // flush the mount-triggered status refresh + (failing) discovery call

        // A stored-but-rejected key must still trigger discovery — proving
        // the no-key gate above doesn't accidentally swallow this case too.
        expect(getProviderModels).toHaveBeenCalled();
        expect(container.textContent).toContain("sk-ant-api03…7f2a");
        expect(container.textContent).toContain("Enter a new key to replace it");
        expect(container.textContent).not.toContain("No API key configured");
    });
});

describe("maxTurnsValidationError / parseMaxTurns", () => {
    it("treats blank as valid — it means 'defer to the backend default'", () => {
        expect(maxTurnsValidationError("")).toBeNull();
        expect(maxTurnsValidationError("   ")).toBeNull();
        expect(parseMaxTurns("")).toBeNull();
        expect(parseMaxTurns("   ")).toBeNull();
    });

    it("rejects 0 and negative values", () => {
        expect(maxTurnsValidationError("0")).not.toBeNull();
        expect(maxTurnsValidationError("-1")).not.toBeNull();
        expect(parseMaxTurns("0")).toBeNull();
        expect(parseMaxTurns("-1")).toBeNull();
    });

    it("rejects non-integer and non-numeric input", () => {
        expect(maxTurnsValidationError("1.5")).not.toBeNull();
        expect(maxTurnsValidationError("abc")).not.toBeNull();
        expect(parseMaxTurns("1.5")).toBeNull();
        expect(parseMaxTurns("abc")).toBeNull();
    });

    it("accepts 1 (the documented minimum) and larger whole numbers, including deliberately large ones", () => {
        expect(maxTurnsValidationError("1")).toBeNull();
        expect(parseMaxTurns("1")).toBe(1);
        expect(maxTurnsValidationError("500")).toBeNull();
        expect(parseMaxTurns("500")).toBe(500);
    });
});

describe("CoordinatorConfigFields Max Turns control", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
    });

    afterEach(async () => {
        await act(async () => {
            root.unmount();
        });
        document.body.removeChild(container);
    });

    function render(value: CoordinatorConfigFieldsValue) {
        return act(async () => {
            root.render(React.createElement(CoordinatorConfigFields, { value, onChange: () => {} }));
        });
    }

    it("renders only in API mode, beside Timeout — CLI mode shows No-output Timeout instead", async () => {
        await render({ ...DEFAULT_COORDINATOR_CONFIG_VALUE, runnerMode: "api" });
        expect(container.querySelector("#ae-max-turns")).toBeTruthy();
        expect(container.querySelector("#ae-noout")).toBeNull();

        await render({ ...DEFAULT_COORDINATOR_CONFIG_VALUE, runnerMode: "cli" });
        expect(container.querySelector("#ae-max-turns")).toBeNull();
        expect(container.querySelector("#ae-noout")).toBeTruthy();
    });

    it("shows the default-value placeholder and no 'unlimited' option", async () => {
        await render({ ...DEFAULT_COORDINATOR_CONFIG_VALUE, runnerMode: "api" });
        const input = container.querySelector("#ae-max-turns") as HTMLInputElement;
        expect(input.placeholder).toBe("default (50)");
        // No checkbox/toggle/select offering an "unlimited" affordance next
        // to the field — it's a plain numeric text input, full stop.
        expect(input.type).toBe("text");
    });

    it("shows inline validation error text for 0, hiding the plain helper text", async () => {
        await render({ ...DEFAULT_COORDINATOR_CONFIG_VALUE, runnerMode: "api", maxTurns: "0" });
        const errorText = container.querySelector("#ae-max-turns")!.parentElement!.textContent ?? "";
        expect(errorText).toMatch(/1 or more/);
    });

    it("shows the plain helper text (not an error) for a valid value", async () => {
        await render({ ...DEFAULT_COORDINATOR_CONFIG_VALUE, runnerMode: "api", maxTurns: "25" });
        const wrapperText = container.querySelector("#ae-max-turns")!.parentElement!.textContent ?? "";
        expect(wrapperText).toMatch(/stops/i);
        expect(wrapperText).not.toMatch(/1 or more/);
    });
});
