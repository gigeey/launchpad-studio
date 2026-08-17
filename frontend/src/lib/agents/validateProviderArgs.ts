/**
 * Detects CLI flags pasted into an agent profile's Advanced Settings Args
 * list that belong to a *different* provider than the one the profile's
 * `command` targets. Mixing these up is a real failure mode: Codex hard-
 * errors on an unrecognized flag ("unexpected argument"), while cursor-agent
 * exits 1 with no output at all — both are confusing to debug from the UI
 * alone, so we warn inline instead of waiting for the crash.
 *
 * This is deliberately a *cross-contamination* check, not a full CLI arg
 * validator: it only flags args we know for certain are exclusive to one
 * provider. An unrecognized arg is left alone (no opinion), so power users
 * can still add novel flags without fighting false positives.
 */

export type KnownProviderCommand = "claude" | "cursor-agent" | "codex" | "agy";

const KNOWN_PROVIDER_COMMANDS: readonly KnownProviderCommand[] = [
    "claude",
    "cursor-agent",
    "codex",
    "agy",
];

/** Flags every one of our shipped templates may use interchangeably — never a signal of cross-contamination. */
const SHARED_FLAGS = new Set(["--print", "--output-format", "--model"]);

interface SignatureFlag {
    /** Providers this flag is valid for. Anything outside this set is a mismatch. */
    validFor: readonly KnownProviderCommand[];
    /** Human-readable label for the owning provider(s), used in the warning copy. */
    ownerLabel: string;
}

// Built from the three shipped templates (frontend/src/data/agentTemplates.ts)
// plus known cross-provider flags that aren't part of the default arg lists
// but get appended by the backend's `build_argv` for a specific provider only
// (crates/ao-engine/src/agent_runner/cli.rs).
const SIGNATURE_FLAGS: Record<string, SignatureFlag> = {
    "--verbose": { validFor: ["claude"], ownerLabel: "Claude" },
    // agy also takes --dangerously-skip-permissions (its non-interactive
    // auto-approve flag), so it's not Claude-exclusive.
    "--dangerously-skip-permissions": { validFor: ["claude", "agy"], ownerLabel: "Claude and agy" },
    "--include-partial-messages": { validFor: ["claude"], ownerLabel: "Claude" },
    "--append-system-prompt": { validFor: ["claude"], ownerLabel: "Claude" },
    "--thinking": { validFor: ["claude"], ownerLabel: "Claude" },
    "--thinking-display": { validFor: ["claude"], ownerLabel: "Claude" },
    "--max-thinking-tokens": { validFor: ["claude"], ownerLabel: "Claude" },
    "--thinking-budget": { validFor: ["claude"], ownerLabel: "Claude" },
    // Backend appends this for anything that isn't Codex (see `build_argv`'s
    // codex/else branch) — valid for Claude and cursor-agent, but Codex has
    // no such flag and hard-errors on it.
    "--mcp-config": { validFor: ["claude", "cursor-agent"], ownerLabel: "Claude and cursor-agent" },
    "--force": { validFor: ["cursor-agent"], ownerLabel: "cursor-agent" },
    "--approve-mcps": { validFor: ["cursor-agent"], ownerLabel: "cursor-agent" },
    "--trust": { validFor: ["cursor-agent"], ownerLabel: "cursor-agent" },
    "--stream-partial-output": { validFor: ["cursor-agent"], ownerLabel: "cursor-agent" },
    "--json": { validFor: ["codex"], ownerLabel: "codex" },
    "--skip-git-repo-check": { validFor: ["codex"], ownerLabel: "codex" },
    "-p": { validFor: ["agy"], ownerLabel: "agy" },
    "--prompt": { validFor: ["agy"], ownerLabel: "agy" },
    "--conversation": { validFor: ["agy"], ownerLabel: "agy" },
    "--effort": { validFor: ["agy"], ownerLabel: "agy" },
    "--mode": { validFor: ["agy"], ownerLabel: "agy" },
    "--sandbox": { validFor: ["agy", "codex"], ownerLabel: "agy and codex" },
    "--add-dir": { validFor: ["agy"], ownerLabel: "agy" },
    "--agent": { validFor: ["agy"], ownerLabel: "agy" },
    "--continue": { validFor: ["agy"], ownerLabel: "agy" },
    "-c": { validFor: ["agy", "codex"], ownerLabel: "agy and codex" },
    "--new-project": { validFor: ["agy"], ownerLabel: "agy" },
    "--project": { validFor: ["agy"], ownerLabel: "agy" },
    "--print-timeout": { validFor: ["agy"], ownerLabel: "agy" },
};

const KNOWN_FAILURE_MODE: Partial<Record<KnownProviderCommand, string>> = {
    codex: 'codex will hard-error with "unexpected argument" and the run will fail',
    "cursor-agent": "cursor-agent will silently exit 1 with no output",
};

function commandBasename(command: string): string {
    const segments = command.split(/[\\/]/);
    return segments[segments.length - 1] || command;
}

function detectProvider(command: string): KnownProviderCommand | null {
    const basename = commandBasename(command);
    return (KNOWN_PROVIDER_COMMANDS as readonly string[]).includes(basename)
        ? (basename as KnownProviderCommand)
        : null;
}

/**
 * Flags args that belong to a different known provider than `command`
 * targets. Returns one human-readable warning per offending flag, in the
 * order the flags appear in `args`. Non-blocking by design — callers should
 * surface these as inline hints, never prevent saving.
 */
export function validateProviderArgs(command: string, args: string[]): string[] {
    const provider = detectProvider(command);
    if (!provider) return [];

    const warnings: string[] = [];
    for (const arg of args) {
        const flag = arg.split("=")[0];
        if (SHARED_FLAGS.has(flag)) continue;

        const signature = SIGNATURE_FLAGS[flag];
        if (!signature || signature.validFor.includes(provider)) continue;

        const failureMode = KNOWN_FAILURE_MODE[provider] ?? `${provider} may not recognize this flag`;
        warnings.push(`"${flag}" belongs to ${signature.ownerLabel}, not ${provider} — ${failureMode}. Remove it.`);
    }
    return warnings;
}
