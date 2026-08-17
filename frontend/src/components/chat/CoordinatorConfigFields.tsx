/**
 * Advanced/coordinator config fields shared by AgentProfileModal and
 * TeamEditModal — both portaled modals, never a page. Every hardcoded CSS
 * var below is deliberately from the `--modal-*` namespace (not the plain
 * `--text-primary` family formPrimitives.tsx defaults to for page contexts)
 * since this component has no page-level consumer to stay compatible with.
 * See the "Color surface" note atop formPrimitives.tsx for why the two
 * namespaces exist and diverge for "chrome" themes.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Check, Loader2 } from "lucide-react";

import type { AgentNativeProvider, AgentProfile, AgentReasoningEffort, AgentRunnerMode } from "../../types/api";
import {
    CLI_TEMPLATES,
    FormSelect,
    INPUT_MODE_OPTIONS,
    KVEditor,
    Label,
    OUTPUT_FORMAT_OPTIONS,
    RUNNER_MODE_OPTIONS,
    StringListEditor,
    TextInput,
    useCliDetection,
} from "./formPrimitives";
import {
    deleteProviderApiKey,
    getProviderModels,
    getProviderStatuses,
    ProviderModelDiscoveryError,
    setProviderApiKey,
} from "../../lib/api";
import type { ProviderStatus } from "../../types/api";
import { validateProviderArgs } from "../../lib/agents/validateProviderArgs";

export interface CoordinatorConfigFieldsValue {
    selectedTemplate: string | null;
    command: string;
    args: string[];
    outputFormat: string;
    inputMode: string;
    normalizer: string;
    modelArg: string;
    systemPromptArg: string;
    sessionArg: string;
    resumeArgs: string[];
    modelAliases: Record<string, string>;
    model: string;
    customModelMode: boolean;
    timeoutSeconds: string;
    noOutputTimeoutMs: string;
    maxInstances: string;
    /** Empty string means "unset" — the backend defers to `DEFAULT_MAX_TURNS`
     *  rather than treating empty as an explicit 0. Relevant only in API mode
     *  (native runner); ignored for CLI-mode agents. */
    maxTurns: string;
    clearEnv: boolean;
    env: Record<string, string>;
    runnerMode: AgentRunnerMode;
    nativeProvider: AgentNativeProvider;
}

// Limited to providers the backend's agent runner actually supports as a
// `native_provider` (`ao_protocol::agent::NativeProvider` has no Gemini
// variant yet) — NOT the full set `providers.toml` can store credentials
// for. Keep this in lockstep with the backend enum; adding an option the
// server can't deserialize would break agent creation.
const NATIVE_PROVIDER_OPTIONS: { value: AgentNativeProvider; label: string }[] = [
    { value: "anthropic", label: "Anthropic (Claude)" },
    { value: "openai", label: "OpenAI (GPT)" },
    { value: "openrouter", label: "OpenRouter" },
];

// Providers `GET /providers/{name}/models` can actually query — mirrors the
// backend's `DISCOVERABLE_PROVIDERS` set in
// `ao-engine-tools-provider-config/src/model_discovery.rs`. No capability
// flag comes back over the wire (`ProviderStatus` doesn't carry one), so
// this is a frontend-side mirror of that backend list rather than a
// server-driven signal — keep the two in lockstep. Every value
// `NATIVE_PROVIDER_OPTIONS` currently offers is discoverable; the predicate
// exists so the model/endpoint controls don't render dead if a
// not-yet-wired provider (e.g. Gemini) is ever added to that list before
// its discovery client lands.
const MODEL_DISCOVERY_CAPABLE_PROVIDERS = new Set<string>(["anthropic", "openai", "openrouter"]);

export function providerSupportsModelDiscovery(provider: string): boolean {
    return MODEL_DISCOVERY_CAPABLE_PROVIDERS.has(provider);
}

// Providers whose native/API client actually forwards a `reasoning_effort`
// value onto the wire — mapped to a `thinking.budget_tokens` value for
// Anthropic, the native `reasoning_effort` string for OpenAI-compatible
// chat completions (`DefaultProviderFactory::build` in
// `crates/ao-engine/src/agent_runner/native.rs`). A distinct predicate from
// {@link providerSupportsModelDiscovery} — the two capabilities are
// unrelated even though today's provider list happens to satisfy both — so
// a future provider that gains discovery without a reasoning channel (or
// vice versa) doesn't have to fake the other. Kept in lockstep with the
// backend by hand, same as the model-discovery set above: no capability
// flag comes back over the wire for this either.
const REASONING_EFFORT_CAPABLE_PROVIDERS = new Set<string>(["anthropic", "openai", "openrouter"]);

export function providerSupportsReasoningEffort(provider: string): boolean {
    return REASONING_EFFORT_CAPABLE_PROVIDERS.has(provider);
}

const REASONING_EFFORT_OPTIONS: { value: string; label: string }[] = [
    { value: "", label: "— none (provider default) —" },
    { value: "low", label: "Low" },
    { value: "medium", label: "Medium" },
    { value: "high", label: "High" },
];

// Mirrors `ao_protocol::agent::DEFAULT_MAX_TURNS` — shown only as a
// placeholder/helper-text hint for the blank-field fallback. Not fetched
// from the backend at render time, so keep this in lockstep by hand if the
// backend constant ever changes.
const DEFAULT_MAX_TURNS_HINT = 50;

/** Validates the raw "Max Turns" field text. Blank is always valid (it means
 *  "unset — defer to the backend default"); a non-blank value must be a
 *  whole number of 1 or more. There is deliberately no "unlimited" sentinel
 *  — a run always has some ceiling, even if a user types a very large one. */
export function maxTurnsValidationError(raw: string): string | null {
    const trimmed = raw.trim();
    if (trimmed === "") return null;
    const parsed = Number(trimmed);
    if (!Number.isInteger(parsed) || parsed < 1) {
        return "Enter a whole number of 1 or more, or leave blank for the default.";
    }
    return null;
}

/** Converts the raw field text into the value the profile-save payload
 *  should carry: `null` for blank (defer to the backend default) or an
 *  invalid entry (fails safe rather than sending garbage — the Save button
 *  is disabled via {@link maxTurnsValidationError} whenever this would
 *  matter), otherwise the parsed integer. */
export function parseMaxTurns(raw: string): number | null {
    const trimmed = raw.trim();
    if (trimmed === "") return null;
    const parsed = Number(trimmed);
    if (!Number.isInteger(parsed) || parsed < 1) return null;
    return parsed;
}

export const DEFAULT_COORDINATOR_CONFIG_VALUE: CoordinatorConfigFieldsValue = {
    selectedTemplate: null,
    command: "echo",
    args: ["Hello from agent"],
    outputFormat: "Text",
    inputMode: "Arg",
    normalizer: "",
    modelArg: "",
    systemPromptArg: "",
    sessionArg: "",
    resumeArgs: [],
    modelAliases: {},
    model: "",
    customModelMode: false,
    timeoutSeconds: "300",
    noOutputTimeoutMs: "30000",
    maxInstances: "1",
    maxTurns: "",
    clearEnv: false,
    env: {},
    runnerMode: "cli",
    nativeProvider: "anthropic",
};

export function coordinatorConfigFromProfile(profile: AgentProfile | undefined): CoordinatorConfigFieldsValue {
    return {
        selectedTemplate: profile?.template ?? null,
        command: profile?.provider?.command ?? "echo",
        args: profile?.provider?.args ?? ["Hello from agent"],
        outputFormat: profile?.provider?.output_format ?? "Text",
        inputMode: profile?.provider?.input_mode ?? "Arg",
        normalizer: profile?.provider?.normalizer ?? "",
        modelArg: profile?.provider?.model_arg ?? "",
        systemPromptArg: profile?.provider?.system_prompt_arg ?? "",
        sessionArg: profile?.provider?.session_arg ?? "",
        resumeArgs: profile?.provider?.resume_args ?? [],
        modelAliases: profile?.provider?.model_aliases ?? {},
        model: profile?.model ?? "",
        customModelMode: !!(profile?.model && profile?.provider?.model_aliases && !Object.keys(profile.provider.model_aliases).includes(profile.model)),
        timeoutSeconds: String(profile?.timeout_seconds ?? 300),
        noOutputTimeoutMs: String(profile?.provider?.no_output_timeout_ms ?? 30000),
        maxInstances: String(profile?.max_instances ?? 1),
        maxTurns: profile?.max_turns != null ? String(profile.max_turns) : "",
        clearEnv: profile?.provider?.clear_env ?? false,
        env: profile?.env ?? {},
        runnerMode: profile?.runner_mode ?? "cli",
        nativeProvider: profile?.native_provider ?? "anthropic",
    };
}

export interface CoordinatorConfigFieldsProps {
    value: CoordinatorConfigFieldsValue;
    onChange: (next: CoordinatorConfigFieldsValue) => void;
    /** Optional id prefix (default "ae-") to avoid collisions when multiple instances render. */
    idPrefix?: string;
    /** When true, all nested form controls are disabled (used by the legacy
     *  coordinator read-only state in TeamEditModal). */
    disabled?: boolean;
    /** When true, the runner-mode dropdown is rendered disabled. Set on edit
     *  paths — runner mode is fixed at agent creation and cannot be flipped
     *  for an existing agent (would orphan its session history). */
    lockRunnerMode?: boolean;
}

export function CoordinatorConfigFields({ value, onChange, idPrefix = "ae-", disabled, lockRunnerMode }: CoordinatorConfigFieldsProps) {
    const cliAvailability = useCliDetection();
    // API-mode agents are driven by NativeAgentRunner (in-process model client),
    // so the CLI provider settings — Command, Args, Output/Input Mode, Normalizer,
    // System-Prompt/Model/Session/Resume Args, Model Aliases, Clear Environment —
    // are no-ops at runtime. We hide them to stop showing dead form fields.
    // Templates are also CLI-only (they exist to auto-fill the dead fields).
    const isApiMode = value.runnerMode === "api";

    const update = (patch: Partial<CoordinatorConfigFieldsValue>) => onChange({ ...value, ...patch });

    // Cross-provider flag guardrail: warns (never blocks) when Args contains
    // a flag known to belong to a *different* CLI provider than `command`
    // targets — e.g. pasting `--mcp-config` into a codex profile, which
    // hard-errors at spawn time instead of failing here where it's visible.
    const argWarnings = useMemo(
        () => (isApiMode ? [] : validateProviderArgs(value.command, value.args)),
        [isApiMode, value.command, value.args],
    );

    const maxTurnsError = useMemo(() => maxTurnsValidationError(value.maxTurns), [value.maxTurns]);

    return (
        <fieldset
            disabled={disabled}
            className={`flex flex-col gap-[20px] border-0 m-0 p-0 min-w-0${disabled ? " opacity-70" : ""}`}
        >
            {/* ── Kind + Template Card ── */}
            <div className="rounded-[12px] border-[1.5px] border-[color-mix(in_srgb,var(--modal-border-secondary)_55%,var(--modal-text-tertiary)_45%)] px-[16px] py-[14px] flex flex-col gap-[14px]">
                <div>
                    <Label htmlFor={`${idPrefix}kind`} className="block mb-[6px]">Kind</Label>
                    <FormSelect
                        id={`${idPrefix}kind`}
                        value={value.runnerMode}
                        onChange={(v) => update({ runnerMode: v as AgentRunnerMode })}
                        options={RUNNER_MODE_OPTIONS}
                        disabled={lockRunnerMode}
                    />
                    <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)] leading-[15px]">
                        CLI mode uses a configured CLI binary. Native (API) mode uses provider APIs directly. Cannot be changed after creation.
                    </p>
                </div>
                {!isApiMode && (
                    <div>
                        <div className="flex items-baseline gap-[10px] mb-[10px]">
                            <Label htmlFor={`${idPrefix}template`} className="shrink-0">Template</Label>
                            <p className="text-[11px] text-[var(--modal-text-secondary)] leading-[16px]">Select a known platform format to auto-fill provider settings.</p>
                        </div>
                        <div className="flex flex-wrap gap-[8px]">
                            <TemplateChip
                                label="Custom"
                                selected={value.selectedTemplate === null}
                                onClick={() => update({ selectedTemplate: null })}
                            />
                            {CLI_TEMPLATES.map((tpl) => {
                                const available = cliAvailability[tpl.id];
                                const isDetecting = available === null;
                                const isSelected = value.selectedTemplate === tpl.id;
                                const isDisabled = available === false;
                                return (
                                    <TemplateChip
                                        key={tpl.id}
                                        label={tpl.label}
                                        selected={isSelected}
                                        disabled={isDisabled}
                                        trailing={
                                            isDetecting ? <Loader2 className="w-[11px] h-[11px] animate-spin text-[var(--modal-text-tertiary)]" />
                                                : available ? <span className="w-[6px] h-[6px] rounded-full bg-green-500" />
                                                    : <span className="text-[10px] text-[var(--modal-text-tertiary)] italic">not found</span>
                                        }
                                        onClick={() => !isDisabled && update({ selectedTemplate: tpl.id })}
                                    />
                                );
                            })}
                        </div>
                    </div>
                )}
            </div>

            {/* ── Provider ── */}
            <SectionHeader>Provider</SectionHeader>
            {isApiMode ? (
                /* API mode: the engine is the provider. The CLI argv-shaping
                 * fields below would never be read by NativeAgentRunner, so
                 * we hide them entirely. Credentials, endpoint, and model
                 * are configured directly below via ProviderConfigFields —
                 * that panel is a first-class editor for providers.toml,
                 * not just a pointer telling the user to go edit the file
                 * themselves. */
                <div className="flex flex-col gap-[12px]">
                    <div>
                        <Label htmlFor={`${idPrefix}native-provider`} className="block mb-[6px]">API provider</Label>
                        <FormSelect
                            id={`${idPrefix}native-provider`}
                            value={value.nativeProvider}
                            onChange={(v) => update({ nativeProvider: v as AgentNativeProvider })}
                            options={NATIVE_PROVIDER_OPTIONS}
                        />
                        <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)] leading-[15px]">
                            Picks which provider client to instantiate.
                        </p>
                    </div>
                    <ProviderConfigFields idPrefix={idPrefix} provider={value.nativeProvider} />
                    <div className="rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] px-[14px] py-[12px] text-[12px] text-[var(--modal-text-secondary)] leading-[18px]">
                        CLI provider settings (command, args, output format, etc.) don't apply in API mode.
                    </div>
                </div>
            ) : (
                <>
                    <div>
                        <Label htmlFor={`${idPrefix}cmd`}>Command</Label>
                        <TextInput id={`${idPrefix}cmd`} value={value.command} onChange={(v) => update({ command: v })} placeholder="e.g. claude" monospace variant="prominent" />
                    </div>
                    <div>
                        <Label htmlFor={`${idPrefix}args`}>Args</Label>
                        <StringListEditor id={`${idPrefix}args`} values={value.args} onChange={(v) => update({ args: v })} placeholder="e.g. --print" variant="prominent" />
                        {argWarnings.length > 0 && (
                            <ul className="mt-[8px] flex flex-col gap-[6px]">
                                {argWarnings.map((warning, i) => (
                                    <li key={i} className="flex items-start gap-[6px] text-[11px] text-amber-600 dark:text-amber-400 leading-[15px]">
                                        <AlertTriangle className="w-[12px] h-[12px] flex-shrink-0 mt-[1.5px]" />
                                        <span>{warning}</span>
                                    </li>
                                ))}
                            </ul>
                        )}
                    </div>
                    <div className="grid grid-cols-2 gap-[12px]">
                        <div>
                            <Label htmlFor={`${idPrefix}out-fmt`}>Output Format</Label>
                            <FormSelect id={`${idPrefix}out-fmt`} value={value.outputFormat} onChange={(v) => update({ outputFormat: v })} options={OUTPUT_FORMAT_OPTIONS} />
                        </div>
                        <div>
                            <Label htmlFor={`${idPrefix}in-mode`}>Input Mode</Label>
                            <FormSelect id={`${idPrefix}in-mode`} value={value.inputMode} onChange={(v) => update({ inputMode: v })} options={INPUT_MODE_OPTIONS} />
                        </div>
                    </div>
                    <div>
                        <Label htmlFor={`${idPrefix}normalizer`}>Normalizer</Label>
                        <TextInput id={`${idPrefix}normalizer`} value={value.normalizer} onChange={(v) => update({ normalizer: v })} placeholder="e.g. claude, cursor-agent" monospace variant="prominent" />
                        <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)]">Normalizer name. Leave empty to auto-detect from command name.</p>
                    </div>
                    <div>
                        <Label htmlFor={`${idPrefix}sp-arg`}>System Prompt Arg</Label>
                        <TextInput id={`${idPrefix}sp-arg`} value={value.systemPromptArg} onChange={(v) => update({ systemPromptArg: v })} placeholder="e.g. --system-prompt" monospace variant="prominent" />
                    </div>
                    <div>
                        <Label htmlFor={`${idPrefix}model-arg`}>Model Arg</Label>
                        <TextInput id={`${idPrefix}model-arg`} value={value.modelArg} onChange={(v) => update({ modelArg: v })} placeholder="e.g. --model" monospace variant="prominent" />
                    </div>
                    <div>
                        <Label htmlFor={`${idPrefix}sess-arg`}>Session Arg</Label>
                        <TextInput id={`${idPrefix}sess-arg`} value={value.sessionArg} onChange={(v) => update({ sessionArg: v })} placeholder="e.g. --session" monospace variant="prominent" />
                    </div>
                    <div>
                        <Label htmlFor={`${idPrefix}resume`}>Resume Args</Label>
                        <StringListEditor id={`${idPrefix}resume`} values={value.resumeArgs} onChange={(v) => update({ resumeArgs: v })} placeholder="e.g. --resume" variant="prominent" />
                    </div>
                    <div>
                        <Label htmlFor={`${idPrefix}aliases`}>Model Aliases</Label>
                        <KVEditor values={value.modelAliases} onChange={(v) => update({ modelAliases: v })} keyPlaceholder="alias" valuePlaceholder="model name" variant="prominent" />
                        <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)]">Friendly aliases that map to actual model IDs (e.g. "fast" → "claude-3-haiku").</p>
                    </div>
                    <div>
                        <Label htmlFor={`${idPrefix}model`}>Model</Label>
                        {Object.keys(value.modelAliases).length > 0 ? (
                            <>
                                <select
                                    id={`${idPrefix}model`}
                                    value={value.customModelMode ? "__custom__" : Object.keys(value.modelAliases).includes(value.model) ? value.model : ""}
                                    onChange={(e) => {
                                        const v = e.target.value;
                                        if (v === "__custom__") update({ customModelMode: true, model: "" });
                                        else update({ customModelMode: false, model: v });
                                    }}
                                    className="w-full h-[42px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-primary)] px-[12px] text-[13px] font-mono text-[var(--modal-text-primary)] focus:outline-none focus:ring-2 focus:ring-[var(--modal-accent)]"
                                >
                                    <option value="">— none (use provider default) —</option>
                                    {Object.entries(value.modelAliases).map(([alias, resolved]) => (
                                        <option key={alias} value={alias}>{alias} → {resolved}</option>
                                    ))}
                                    <option value="__custom__">Custom...</option>
                                </select>
                                {value.customModelMode && (
                                    <div className="mt-[6px]">
                                        <TextInput id={`${idPrefix}model-custom`} value={value.model} onChange={(v) => update({ model: v })} placeholder="e.g. claude-sonnet-4-20250514" monospace variant="prominent" />
                                    </div>
                                )}
                            </>
                        ) : (
                            <TextInput id={`${idPrefix}model`} value={value.model} onChange={(v) => update({ model: v })} placeholder="e.g. claude-sonnet-4-20250514" monospace variant="prominent" />
                        )}
                    </div>
                </>
            )}

            {/* ── Runtime ── */}
            <SectionHeader>Runtime</SectionHeader>
            <div className="grid grid-cols-2 gap-[12px]">
                <div>
                    <Label htmlFor={`${idPrefix}timeout`}>Timeout (s)</Label>
                    <TextInput id={`${idPrefix}timeout`} value={value.timeoutSeconds} onChange={(v) => update({ timeoutSeconds: v })} placeholder="300" monospace variant="prominent" />
                </div>
                {isApiMode ? (
                    <div>
                        <Label htmlFor={`${idPrefix}max-turns`}>Max Turns</Label>
                        {/* Empty = defer to the backend's DEFAULT_MAX_TURNS
                            (ao_protocol::agent::DEFAULT_MAX_TURNS) — the
                            placeholder number below is only a hint for that
                            fallback; it is never sent as a value. There is no
                            "unlimited" option: a run always has a ceiling. */}
                        <TextInput
                            id={`${idPrefix}max-turns`}
                            value={value.maxTurns}
                            onChange={(v) => update({ maxTurns: v })}
                            placeholder={`default (${DEFAULT_MAX_TURNS_HINT})`}
                            monospace
                            variant="prominent"
                        />
                        {maxTurnsError ? (
                            <p className="mt-[6px] text-[11px] text-[var(--error)] leading-[15px]">{maxTurnsError}</p>
                        ) : (
                            <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)] leading-[15px]">
                                The run stops once it hits this many model turns, so a stuck agent can't keep making unbounded calls against your API key. Leave blank to use the default of {DEFAULT_MAX_TURNS_HINT}.
                            </p>
                        )}
                    </div>
                ) : (
                    <div>
                        <Label htmlFor={`${idPrefix}noout`}>No-output Timeout (ms)</Label>
                        <TextInput id={`${idPrefix}noout`} value={value.noOutputTimeoutMs} onChange={(v) => update({ noOutputTimeoutMs: v })} placeholder="30000" monospace variant="prominent" />
                    </div>
                )}
            </div>
            <div>
                <Label htmlFor={`${idPrefix}max-inst`}>Max Instances</Label>
                <TextInput id={`${idPrefix}max-inst`} value={value.maxInstances} onChange={(v) => update({ maxInstances: v })} placeholder="1" monospace variant="prominent" />
            </div>
            {!isApiMode && (
                <div className="flex items-center justify-between">
                    <div>
                        <p className="text-[13px] font-medium text-[var(--modal-text-primary)]">Clear Environment</p>
                        <p className="text-[11px] text-[var(--modal-text-secondary)]">Start the process with a clean env</p>
                    </div>
                    <button
                        type="button" role="switch" aria-checked={value.clearEnv}
                        onClick={() => update({ clearEnv: !value.clearEnv })}
                        className={`relative w-[40px] h-[24px] rounded-full transition-colors duration-200 cursor-pointer ${value.clearEnv ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-secondary)]"}`}
                    >
                        <span className={`absolute top-[3px] left-[3px] w-[18px] h-[18px] rounded-full bg-white shadow-sm transition-transform duration-200 ${value.clearEnv ? "translate-x-[16px]" : "translate-x-0"}`} />
                    </button>
                </div>
            )}
            <div>
                <Label htmlFor={`${idPrefix}env`}>Environment Variables</Label>
                <KVEditor values={value.env} onChange={(v) => update({ env: v })} keyPlaceholder="VAR_NAME" valuePlaceholder="value" variant="prominent" />
            </div>
        </fieldset>
    );
}

// ─── small helpers ─────────────────────────────────────────────────────────────

function SectionHeader({ children }: { children: React.ReactNode }) {
    return (
        <div className="flex items-center gap-[10px] pt-[6px]">
            <p className="text-[13px] font-semibold text-[var(--modal-text-primary)] uppercase tracking-wide">{children}</p>
            <div className="flex-1 h-px bg-[var(--modal-border-secondary)]" />
        </div>
    );
}

function TemplateChip({ label, selected, disabled, trailing, onClick }: {
    label: string;
    selected?: boolean;
    disabled?: boolean;
    trailing?: React.ReactNode;
    onClick: () => void;
}) {
    return (
        <button
            type="button"
            onClick={onClick}
            disabled={disabled}
            className={`flex items-center gap-[8px] h-[36px] px-[12px] rounded-[8px] border text-[13px] font-medium transition-colors ${disabled
                ? "opacity-40 cursor-not-allowed border-[var(--modal-border-secondary)] text-[var(--modal-text-secondary)]"
                : selected
                    ? "bg-[#1164A3] border-[#1164A3] text-white cursor-pointer"
                    : "bg-[var(--modal-bg)] border-[var(--modal-border-secondary)] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] cursor-pointer"
                }`}
        >
            <div className={`w-[16px] h-[16px] rounded-[4px] border-2 flex items-center justify-center flex-shrink-0 ${selected ? "border-white bg-white/20" : "border-[var(--modal-border-secondary)]"
                }`}>
                {selected && <Check className="w-[10px] h-[10px] text-white" />}
            </div>
            <span>{label}</span>
            {trailing && <span className="flex items-center">{trailing}</span>}
        </button>
    );
}

/** Debounce window between the user's last keystroke in the API-key field
 *  and the implicit key-validation call (point 3 of the provider-config
 *  UI spec). */
const KEY_VALIDATION_DEBOUNCE_MS = 250;

/**
 * Credential + model/endpoint editor for the currently-selected API
 * provider. Writes through to `providers.toml` on the backend (`PUT
 * /providers/{name}`, sending `api_key`, `base_url`, and `model` together)
 * so this panel is a first-class editor for those fields, not just a
 * pointer telling the user to go hand-edit the file — a save here and a
 * hand-edit of the file stay in sync because the write merges into the
 * existing section rather than replacing it.
 *
 * The stored key is never read back: `GET /providers` (via
 * `getProviderStatuses`) only reports whether a key is configured, so the
 * key input always renders empty and shows a "configured" indicator instead
 * of ever pre-filling the real secret. `base_url`/`model` aren't secret, so
 * those two pre-fill from the last-saved values.
 *
 * Key validation is implicit rather than a "Test Connection" button: typing
 * into the key field fires `GET /providers/{name}/models`
 * ({@link KEY_VALIDATION_DEBOUNCE_MS}ms after the user stops typing) after
 * transparently persisting the in-progress key first — that endpoint only
 * ever tests whichever key is currently *stored*, there's no "validate a
 * candidate key without saving it" call. The same request doubles as the
 * model-discovery fetch that populates the dropdown below. A 401/403 shows
 * a soft, non-blocking amber warning; it never reverts or blocks the save
 * that already happened.
 *
 * A provider with no stored key at all is a distinct, non-error state: this
 * component never calls `GET /providers/{name}/models` while `GET
 * /providers` reports no key (that call would just fail with the server's
 * "no stored API key" precondition response before ever reaching the
 * network), and renders a neutral invitation instead of a discovery-failure
 * message — see the `hasKey` gate on the mount effect and around {@link
 * discoveryError} below. This keeps "never configured yet" from reading as
 * "broken" on a first-run, all-providers-empty settings panel.
 *
 * The model/endpoint controls only render for providers
 * {@link providerSupportsModelDiscovery} recognizes — right now that's
 * every provider this form's own selector offers, but the gate stays so a
 * future, not-yet-discoverable provider doesn't grow dead controls.
 */
function ProviderConfigFields({ idPrefix, provider }: { idPrefix: string; provider: AgentNativeProvider }) {
    const [status, setStatus] = useState<ProviderStatus | null>(null);
    const [loadingStatus, setLoadingStatus] = useState(true);
    const [keyInput, setKeyInput] = useState("");
    const [baseUrlInput, setBaseUrlInput] = useState("");
    const [modelInput, setModelInput] = useState("");
    const [maxOutputTokensInput, setMaxOutputTokensInput] = useState("");
    const [maxContextTokensInput, setMaxContextTokensInput] = useState("");
    const [reasoningEffortInput, setReasoningEffortInput] = useState<AgentReasoningEffort | "">("");
    const [saving, setSaving] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [justSaved, setJustSaved] = useState(false);

    const [discoveredModels, setDiscoveredModels] = useState<string[]>([]);
    const [discovering, setDiscovering] = useState(false);
    const [authWarning, setAuthWarning] = useState(false);
    // Set for any discovery failure other than auth (network/timeout,
    // malformed upstream response) — those get a specific, actionable
    // string rather than the raw server error text. `authWarning` above
    // stays the sole signal for the auth case since it renders differently
    // (amber "double-check the key" warning vs. this neutral note).
    const [discoveryError, setDiscoveryError] = useState<string | null>(null);

    const canDiscover = providerSupportsModelDiscovery(provider);
    const canReason = providerSupportsReasoningEffort(provider);

    // Returns the freshly-fetched status (or `null` on failure/absence) so
    // the mount effect below can decide whether to bother querying
    // `GET /providers/{name}/models` at all without a second read of state
    // that may not have flushed yet.
    const refresh = useCallback(async (): Promise<ProviderStatus | null> => {
        setLoadingStatus(true);
        setError(null);
        try {
            const statuses = await getProviderStatuses();
            const found = statuses.find((s) => s.provider === provider) ?? null;
            setStatus(found);
            setBaseUrlInput(found?.base_url ?? "");
            setModelInput(found?.model ?? "");
            setMaxOutputTokensInput(found?.max_output_tokens != null ? String(found.max_output_tokens) : "");
            setMaxContextTokensInput(found?.max_context_tokens != null ? String(found.max_context_tokens) : "");
            setReasoningEffortInput(found?.reasoning_effort ?? "");
            return found;
        } catch (err) {
            setError(err instanceof Error ? err.message : "Failed to load provider status");
            return null;
        } finally {
            setLoadingStatus(false);
        }
    }, [provider]);

    // Live model discovery — also this app's only API-key validity check.
    // A 401/403 sets a soft warning; every other failure surfaces a neutral
    // note (below) but otherwise just leaves the dropdown empty — discovery
    // is a convenience, never a requirement, so nothing here ever blocks
    // saving or disables the custom-model field.
    const discoverModels = useCallback(async () => {
        if (!canDiscover) return;
        setDiscovering(true);
        try {
            const models = await getProviderModels(provider);
            setDiscoveredModels(models);
            setAuthWarning(false);
            setDiscoveryError(null);
        } catch (err) {
            const code = err instanceof ProviderModelDiscoveryError ? err.code : undefined;
            setAuthWarning(code === "auth_failure");
            setDiscoveryError(
                code === "auth_failure"
                    ? null
                    : code === "network_failure"
                      ? "Couldn't reach the provider to check for models — it may be unreachable, or timed out."
                      : code === "malformed_response"
                        ? "The provider returned an unexpected response while listing models."
                        : "Couldn't load the model list.",
            );
        } finally {
            setDiscovering(false);
        }
    }, [provider, canDiscover]);

    // Persists api_key + base_url + model together (the backend route
    // requires all three on every write). `silent` is used by the debounced
    // key-validation flow below: no spinner/"Saved" flash, and the key
    // input is left as-is so it doesn't vanish out from under the user
    // mid-typing. Never gated on `discoverModels()`'s outcome — that call
    // happens *after* persisting succeeds, purely to populate the dropdown
    // and surface a soft warning, so an invalid key never blocks the save.
    const persist = useCallback(
        async (opts?: { silent?: boolean }) => {
            const trimmedKey = keyInput.trim();
            if (!trimmedKey) return;
            if (!opts?.silent) {
                setSaving(true);
                setError(null);
            }
            const parsedMaxOutputTokens = maxOutputTokensInput.trim() === "" ? undefined : Number(maxOutputTokensInput.trim());
            const parsedMaxContextTokens = maxContextTokensInput.trim() === "" ? undefined : Number(maxContextTokensInput.trim());
            try {
                await setProviderApiKey(provider, trimmedKey, {
                    baseUrl: baseUrlInput.trim() || undefined,
                    model: modelInput.trim() || undefined,
                    maxOutputTokens: Number.isFinite(parsedMaxOutputTokens) ? parsedMaxOutputTokens : undefined,
                    maxContextTokens: Number.isFinite(parsedMaxContextTokens) ? parsedMaxContextTokens : undefined,
                    reasoningEffort: canReason && reasoningEffortInput ? reasoningEffortInput : undefined,
                });
                if (!opts?.silent) {
                    setKeyInput("");
                    setJustSaved(true);
                    setTimeout(() => setJustSaved(false), 2000);
                }
                await refresh();
                void discoverModels();
            } catch (err) {
                if (!opts?.silent) setError(err instanceof Error ? err.message : "Failed to save provider settings");
            } finally {
                if (!opts?.silent) setSaving(false);
            }
        },
        [
            provider,
            keyInput,
            baseUrlInput,
            modelInput,
            maxOutputTokensInput,
            maxContextTokensInput,
            reasoningEffortInput,
            canReason,
            refresh,
            discoverModels,
        ],
    );

    useEffect(() => {
        setKeyInput("");
        setDiscoveredModels([]);
        setAuthWarning(false);
        setDiscoveryError(null);
        void (async () => {
            const found = await refresh();
            // Query with whatever key may already be stored from a prior
            // session, so the dropdown can populate without the user
            // retyping an already-valid key. Skip entirely when there's no
            // stored key — the endpoint would just reject the call with its
            // "no stored API key" precondition response before ever
            // reaching the network, which is not a failure worth surfacing
            // for a provider nobody has configured yet.
            if (found?.has_api_key) {
                void discoverModels();
            }
        })();
        // refresh/discoverModels are stable for a given `provider`.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [provider]);

    // Implicit key validation (point 3): debounce_MS after the user stops
    // typing a new key, silently persist + re-run discovery. Deliberately
    // keyed on `keyInput` alone — editing the endpoint/model fields doesn't
    // re-trigger this, only typing a key does.
    useEffect(() => {
        if (!keyInput.trim()) return;
        const timer = setTimeout(() => {
            void persist({ silent: true });
        }, KEY_VALIDATION_DEBOUNCE_MS);
        return () => clearTimeout(timer);
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [keyInput]);

    const handleClear = async () => {
        setSaving(true);
        setError(null);
        try {
            await deleteProviderApiKey(provider);
            setDiscoveredModels([]);
            setAuthWarning(false);
            setDiscoveryError(null);
            await refresh();
        } catch (err) {
            setError(err instanceof Error ? err.message : "Failed to clear API key");
        } finally {
            setSaving(false);
        }
    };

    const hasKey = status?.has_api_key ?? false;
    const fingerprint = status?.api_key_fingerprint ?? null;
    const providerLabel = NATIVE_PROVIDER_OPTIONS.find((o) => o.value === provider)?.label ?? provider;
    const keyFieldId = `${idPrefix}provider-api-key`;
    const baseUrlId = `${idPrefix}provider-base-url`;
    const modelSelectId = `${idPrefix}provider-model-select`;
    const modelCustomId = `${idPrefix}provider-model-custom`;
    const maxOutputTokensId = `${idPrefix}provider-max-output-tokens`;
    const maxContextTokensId = `${idPrefix}provider-max-context-tokens`;
    const reasoningEffortId = `${idPrefix}provider-reasoning-effort`;

    return (
        <div className="flex flex-col gap-[12px]">
            <div>
                <div className="flex items-center justify-between mb-[6px]">
                    <Label htmlFor={keyFieldId} className="!mb-0">API Key</Label>
                    {!loadingStatus && (
                        <span className={`flex items-center gap-[6px] text-[11px] ${hasKey ? "text-green-500" : "text-[var(--modal-text-tertiary)]"}`}>
                            <span className={`w-[6px] h-[6px] rounded-full ${hasKey ? "bg-green-500" : "bg-[var(--modal-text-tertiary)]"}`} />
                            {hasKey ? "Key configured" : "No key configured"}
                        </span>
                    )}
                </div>
                <div className="flex items-center gap-[8px]">
                    <input
                        id={keyFieldId}
                        type="password"
                        value={keyInput}
                        onChange={(e) => setKeyInput(e.target.value)}
                        placeholder={
                            fingerprint
                                ? `${fingerprint} (enter a new key to replace)`
                                : hasKey
                                  ? "•••••••••••••••• (enter a new key to replace)"
                                  : "sk-..."
                        }
                        autoCorrect="off" autoCapitalize="off" spellCheck={false}
                        className="flex-1 min-w-0 h-[42px] px-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[14px] font-mono text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-all"
                    />
                    <button
                        type="button"
                        onClick={() => void persist()}
                        disabled={saving || !keyInput.trim()}
                        className="h-[42px] px-[14px] rounded-[10px] text-[13px] font-semibold bg-[var(--modal-accent)] text-white disabled:opacity-50 disabled:cursor-not-allowed hover:opacity-90 transition-opacity cursor-pointer flex items-center gap-[6px] flex-shrink-0"
                    >
                        {saving && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
                        {saving ? "Saving…" : justSaved ? "Saved" : "Save"}
                    </button>
                    {hasKey && (
                        <button
                            type="button"
                            onClick={handleClear}
                            disabled={saving}
                            className="h-[42px] px-[10px] rounded-[10px] text-[12px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] disabled:opacity-50 disabled:cursor-not-allowed transition-colors cursor-pointer flex-shrink-0"
                        >
                            Clear
                        </button>
                    )}
                </div>
                {error && <p className="mt-[6px] text-[11px] text-[var(--error)]">{error}</p>}
                {authWarning && (
                    <p className="mt-[6px] flex items-start gap-[6px] text-[11px] text-amber-600 dark:text-amber-400 leading-[15px]">
                        <AlertTriangle className="w-[12px] h-[12px] flex-shrink-0 mt-[1.5px]" />
                        <span>
                            {fingerprint
                                ? `${providerLabel} rejected the saved key ${fingerprint} (401). Enter a new key to replace it.`
                                : "The provider rejected this key while checking available models. You can still save it — double-check the key if this looks wrong."}
                        </span>
                    </p>
                )}
                <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)] leading-[15px]">
                    Saved here or hand-edited in{" "}
                    <code className="bg-[var(--modal-bg-input)] px-[3px] py-[1px] rounded text-[10px]">providers.toml</code>
                    {" "}under <code className="bg-[var(--modal-bg-input)] px-[3px] py-[1px] rounded text-[10px]">$LAUNCHPAD_STUDIO_DATA_DIR</code> — both stay in sync. The key is never shown here once saved; typing a new one validates it automatically a moment after you stop typing.
                </p>
            </div>

            {canDiscover && (
                <>
                    <div>
                        <Label htmlFor={baseUrlId} className="block mb-[6px]">API endpoint (optional)</Label>
                        <TextInput
                            id={baseUrlId}
                            value={baseUrlInput}
                            onChange={setBaseUrlInput}
                            placeholder="e.g. http://localhost:11434/v1 — leave empty for the provider's default"
                            monospace
                            variant="prominent"
                        />
                        <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)] leading-[15px]">
                            Point this at a self-hosted or compatible endpoint (Ollama, LM Studio, a proxy, ...). Saved together with the key above.
                        </p>
                    </div>

                    <div>
                        <Label htmlFor={modelCustomId} className="block mb-[6px]">Model</Label>
                        <div className="grid grid-cols-2 gap-[8px]">
                            <FormSelect
                                id={modelSelectId}
                                value={discoveredModels.includes(modelInput) ? modelInput : ""}
                                onChange={(v) => { if (v) setModelInput(v); }}
                                options={[
                                    {
                                        value: "",
                                        label: discovering
                                            ? "Loading models…"
                                            : discoveredModels.length > 0
                                                ? "Select a discovered model…"
                                                : "No models discovered yet",
                                    },
                                    ...discoveredModels.map((m) => ({ value: m, label: m })),
                                ]}
                                disabled={discovering || discoveredModels.length === 0}
                            />
                            <TextInput
                                id={modelCustomId}
                                value={modelInput}
                                onChange={setModelInput}
                                placeholder="Custom model ID"
                                monospace
                                variant="prominent"
                            />
                        </div>
                        <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)] leading-[15px]">
                            The dropdown lists models discovered from the provider's API. The field beside it always accepts an arbitrary model ID, whether or not discovery succeeded. Leave both empty to use the provider's default.
                        </p>
                        {!loadingStatus && !hasKey ? (
                            // State 1: no key stored at all. This is the
                            // default, expected state for every provider on
                            // first launch — an invitation, not a failure —
                            // so it deliberately skips the AlertTriangle icon
                            // and error/warning coloring the two states below
                            // use, and never carries an HTTP status code
                            // (discovery was never attempted; see the mount
                            // effect above).
                            <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)] leading-[15px]">
                                No API key configured — paste one to enable {providerLabel} models.
                            </p>
                        ) : (
                            discoveryError &&
                            !discovering && (
                                // State 3: a key is stored but discovery
                                // failed for some other reason (network,
                                // malformed response, ...). Keeps the real
                                // error surface, unlike state 1 above.
                                <p className="mt-[6px] flex items-start gap-[6px] text-[11px] text-[var(--modal-text-secondary)] leading-[15px]">
                                    <AlertTriangle className="w-[12px] h-[12px] flex-shrink-0 mt-[1.5px]" />
                                    <span>{discoveryError} You can still enter a model ID manually above.</span>
                                </p>
                            )
                        )}
                    </div>
                </>
            )}

            {/* Max output tokens / max context tokens are supported by every
             * provider this form offers today (each provider request builder
             * either sends them on the wire or enforces them client-side),
             * so — unlike the model/endpoint controls above — they render
             * unconditionally rather than behind a capability gate. */}
            <div className="grid grid-cols-2 gap-[12px]">
                <div>
                    <Label htmlFor={maxOutputTokensId} className="block mb-[6px]">Max output tokens</Label>
                    <TextInput
                        id={maxOutputTokensId}
                        value={maxOutputTokensInput}
                        onChange={setMaxOutputTokensInput}
                        placeholder="provider default"
                        monospace
                        variant="prominent"
                    />
                </div>
                <div>
                    <Label htmlFor={maxContextTokensId} className="block mb-[6px]">Max context tokens</Label>
                    <TextInput
                        id={maxContextTokensId}
                        value={maxContextTokensInput}
                        onChange={setMaxContextTokensInput}
                        placeholder="no cap"
                        monospace
                        variant="prominent"
                    />
                </div>
            </div>
            <p className="text-[11px] text-[var(--modal-text-secondary)] leading-[15px] -mt-[6px]">
                Leave either blank to defer to the provider's own default. Max context tokens is an approximate budget — older conversation history is trimmed client-side once the estimate exceeds it; neither provider's API exposes this as a request parameter of its own.
            </p>

            {canReason && (
                <div>
                    <Label htmlFor={reasoningEffortId} className="block mb-[6px]">Reasoning effort</Label>
                    <FormSelect
                        id={reasoningEffortId}
                        value={reasoningEffortInput}
                        onChange={(v) => setReasoningEffortInput(v as AgentReasoningEffort | "")}
                        options={REASONING_EFFORT_OPTIONS}
                    />
                    <p className="mt-[6px] text-[11px] text-[var(--modal-text-secondary)] leading-[15px]">
                        How hard the model should think before responding — mapped onto Anthropic's extended-thinking budget or OpenAI's native reasoning effort field, depending on the provider above.
                    </p>
                </div>
            )}
        </div>
    );
}
