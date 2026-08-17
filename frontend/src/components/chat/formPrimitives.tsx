/**
 * Shared form primitives for agent / team configuration UIs.
 *
 * These low-level controls (Label, TextInput, FormTextarea, FormSelect,
 * KVEditor, StringListEditor) are reused across the AgentProfileModal,
 * CoordinatorConfigFields (inside teams), TeamEditModal, and TeamCreationForm.
 * Extracted here so the form-builder surface stays decoupled from any single
 * modal layout, and so the styling stays consistent without each consumer
 * re-implementing inputs.
 *
 * Color surface: AgentProfileModal/TeamEditModal (and CoordinatorConfigFields,
 * which only ever renders inside those two) are portaled modals — they must
 * read the `--modal-*` CSS variable namespace, never the plain `--text-primary`
 * family, because modals force a light surface for every "chrome" theme
 * regardless of the app's own light/dark mode (see the modal-vars comment in
 * AppShell.tsx). TeamCreationForm is a plain page (NewTeamView/EditTeamView),
 * where the plain family is correct since it tracks the content panel's own
 * scoped light/dark override. Rather than have every one of these controls'
 * ~30 call sites remember which namespace applies, callers wrap their
 * subtree once in <FormSurfaceProvider surface="modal"> (both modal roots
 * already do this) and every primitive below reads it via useFormSurface().
 * Pages don't wrap anything and get the "page" default for free.
 *
 * The CLI template metadata + useCliDetection hook live here too because the
 * template chip strip is shared between the agent and team coordinator
 * editors.
 */
import { createContext, useContext, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ChevronDown, Plus, X } from "lucide-react";

// ─── color surface context ────────────────────────────────────────────────────

type FormSurface = "page" | "modal";

const FormSurfaceContext = createContext<FormSurface>("page");

/** Wrap a modal's form body in this so every Label/TextInput/etc. below it
 *  reads the `--modal-*` CSS var namespace instead of the plain one. See the
 *  file-header comment above for why this matters. */
export function FormSurfaceProvider({ surface, children }: { surface: FormSurface; children: React.ReactNode }) {
    return <FormSurfaceContext.Provider value={surface}>{children}</FormSurfaceContext.Provider>;
}

function useFormSurface(): FormSurface {
    return useContext(FormSurfaceContext);
}

// ─── primitive sub-components ─────────────────────────────────────────────────

const LABEL_STYLES: Record<FormSurface, string> = {
    page: "text-[14px] font-semibold text-[var(--text-primary)]",
    modal: "text-[14px] font-semibold text-[var(--modal-text-label)]",
};

export function Label({ htmlFor, children, className }: { htmlFor: string; children: React.ReactNode; className?: string }) {
    const surface = useFormSurface();
    return (
        <label htmlFor={htmlFor} className={`${LABEL_STYLES[surface]} ${className ?? "block mb-[6px]"}`}>
            {children}
        </label>
    );
}

const TEXT_INPUT_STYLES: Record<"default" | "prominent", Record<FormSurface, string>> = {
    default: {
        page: "w-full h-[42px] px-[12px] rounded-[10px] border border-[var(--border-secondary)] bg-[var(--bg-input)] text-[14px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] outline-none focus:border-[var(--accent)] focus:shadow-[0_0_0_1px_var(--accent)] transition-all disabled:opacity-50 disabled:cursor-not-allowed",
        modal: "w-full h-[42px] px-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[14px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-all disabled:opacity-50 disabled:cursor-not-allowed",
    },
    prominent: {
        page: "w-full h-[43px] px-[12px] rounded-[12px] border-[1.5px] border-[color-mix(in_srgb,var(--border-secondary)_55%,var(--text-tertiary)_45%)] text-[16px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] outline-none focus:border-[var(--accent)] focus:shadow-[0_0_0_4px_color-mix(in_srgb,var(--accent)_22%,transparent)] transition-all disabled:opacity-60",
        modal: "w-full h-[43px] px-[12px] rounded-[12px] border-[1.5px] border-[color-mix(in_srgb,var(--modal-border-secondary)_55%,var(--modal-text-tertiary)_45%)] text-[16px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_4px_color-mix(in_srgb,var(--modal-accent)_22%,transparent)] transition-all disabled:opacity-60",
    },
};

export function TextInput({ id, value, onChange, placeholder, required, monospace, disabled, autoFocus, variant = "default" }: {
    id: string; value: string; onChange: (v: string) => void;
    placeholder?: string; required?: boolean; monospace?: boolean; disabled?: boolean; autoFocus?: boolean;
    /** "prominent" matches RenameThreadModal's field treatment (taller, larger
     *  text, borderless fill, soft focus glow) — opt in per-field rather than
     *  changing the shared default, since other TextInput consumers
     *  (CoordinatorConfigFields, TeamEditModal, TeamCreationForm) haven't been
     *  restyled yet. */
    variant?: "default" | "prominent";
}) {
    const surface = useFormSurface();
    return (
        <input
            id={id} type="text" value={value} required={required} disabled={disabled} autoFocus={autoFocus}
            onChange={(e) => onChange(e.target.value)} placeholder={placeholder}
            autoCorrect="off" autoCapitalize="off" spellCheck={false}
            className={`${TEXT_INPUT_STYLES[variant][surface]} ${monospace ? "font-mono" : ""}`}
        />
    );
}

const TEXTAREA_STYLES: Record<FormSurface, string> = {
    page: "w-full px-[12px] py-[10px] rounded-[8px] border border-[var(--border-secondary)] bg-[var(--bg-input)] text-[14px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] outline-none focus:border-[var(--accent)] focus:shadow-[0_0_0_1px_var(--accent)] transition-all resize-none leading-relaxed",
    modal: "w-full px-[12px] py-[10px] rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[14px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-all resize-none leading-relaxed",
};

export function FormTextarea({ id, value, onChange, placeholder, rows = 4, monospace, fill }: {
    id: string; value: string; onChange: (v: string) => void;
    placeholder?: string; rows?: number; monospace?: boolean; fill?: boolean;
}) {
    const surface = useFormSurface();
    return (
        <textarea
            id={id} value={value} {...(fill ? {} : { rows })}
            onChange={(e) => onChange(e.target.value)} placeholder={placeholder}
            autoCorrect="off" autoCapitalize="off" spellCheck={false}
            className={`${TEXTAREA_STYLES[surface]} ${fill ? "flex-1 min-h-0" : ""} ${monospace ? "font-mono text-[13px]" : ""}`}
        />
    );
}

const FORM_SELECT_STYLES: Record<FormSurface, { select: string; chevron: string }> = {
    page: {
        select: "w-full h-[42px] pl-[12px] pr-[36px] rounded-[10px] border border-[var(--border-secondary)] bg-[var(--bg-input)] text-[14px] text-[var(--text-primary)] outline-none focus:border-[var(--accent)] focus:shadow-[0_0_0_1px_var(--accent)] transition-all appearance-none",
        chevron: "text-[var(--text-secondary)]",
    },
    modal: {
        select: "w-full h-[42px] pl-[12px] pr-[36px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[14px] text-[var(--modal-text-primary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-all appearance-none",
        chevron: "text-[var(--modal-text-secondary)]",
    },
};

export function FormSelect({ id, value, onChange, options, disabled }: {
    id: string; value: string; onChange: (v: string) => void;
    options: { label: string; value: string }[];
    disabled?: boolean;
}) {
    const surface = useFormSurface();
    const styles = FORM_SELECT_STYLES[surface];
    return (
        <div className="relative">
            <select id={id} value={value} onChange={(e) => onChange(e.target.value)} disabled={disabled}
                className={`${styles.select} ${disabled ? "opacity-50 cursor-not-allowed" : "cursor-pointer"}`}
            >
                {options.map((o) => <option key={o.value} value={o.value}>{o.label}</option>)}
            </select>
            <ChevronDown className={`pointer-events-none absolute right-[12px] top-1/2 -translate-y-1/2 w-[15px] h-[15px] ${styles.chevron}`} />
        </div>
    );
}

const STRING_LIST_STYLES: Record<"default" | "prominent", Record<FormSurface, { chip: string; remove: string; input: string; add: string }>> = {
    default: {
        page: {
            chip: "flex-1 h-[42px] px-[12px] rounded-[10px] bg-[var(--bg-tertiary)] text-[13px] font-mono text-[var(--text-primary)] flex items-center truncate",
            remove: "w-[28px] h-[28px] rounded-[7px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--error-bg)] hover:text-[var(--error)] transition-colors cursor-pointer",
            input: "flex-1 h-[42px] px-[12px] rounded-[10px] border border-[var(--border-secondary)] bg-[var(--bg-input)] text-[13px] font-mono text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] outline-none focus:border-[var(--accent)] focus:shadow-[0_0_0_1px_var(--accent)] transition-all",
            add: "h-[42px] px-[12px] rounded-[10px] bg-[var(--bg-hover)] text-[var(--text-primary)] text-[13px] font-medium hover:bg-[var(--border-primary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors flex items-center gap-1 cursor-pointer",
        },
        modal: {
            chip: "flex-1 h-[42px] px-[12px] rounded-[10px] bg-[var(--modal-bg-tertiary)] text-[13px] font-mono text-[var(--modal-text-primary)] flex items-center truncate",
            remove: "w-[28px] h-[28px] rounded-[7px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--error-bg)] hover:text-[var(--error)] transition-colors cursor-pointer",
            input: "flex-1 h-[42px] px-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[13px] font-mono text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-all",
            add: "h-[42px] px-[12px] rounded-[10px] bg-[var(--modal-bg-hover)] text-[var(--modal-text-primary)] text-[13px] font-medium hover:bg-[var(--modal-border-primary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors flex items-center gap-1 cursor-pointer",
        },
    },
    prominent: {
        page: {
            chip: "flex-1 h-[43px] px-[12px] rounded-[12px] bg-[var(--bg-tertiary)] text-[16px] font-mono text-[var(--text-primary)] flex items-center truncate",
            remove: "w-[28px] h-[28px] rounded-[7px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--error-bg)] hover:text-[var(--error)] transition-colors cursor-pointer",
            input: "flex-1 h-[43px] px-[12px] rounded-[12px] border-[1.5px] border-[color-mix(in_srgb,var(--border-secondary)_55%,var(--text-tertiary)_45%)] text-[16px] font-mono text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] outline-none focus:border-[var(--accent)] focus:shadow-[0_0_0_4px_color-mix(in_srgb,var(--accent)_22%,transparent)] transition-all",
            add: "h-[43px] px-[12px] rounded-[12px] bg-[var(--bg-hover)] text-[var(--text-primary)] text-[13px] font-medium hover:bg-[var(--border-primary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors flex items-center gap-1 cursor-pointer",
        },
        modal: {
            chip: "flex-1 h-[43px] px-[12px] rounded-[12px] bg-[var(--modal-bg-tertiary)] text-[16px] font-mono text-[var(--modal-text-primary)] flex items-center truncate",
            remove: "w-[28px] h-[28px] rounded-[7px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--error-bg)] hover:text-[var(--error)] transition-colors cursor-pointer",
            input: "flex-1 h-[43px] px-[12px] rounded-[12px] border-[1.5px] border-[color-mix(in_srgb,var(--modal-border-secondary)_55%,var(--modal-text-tertiary)_45%)] text-[16px] font-mono text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_4px_color-mix(in_srgb,var(--modal-accent)_22%,transparent)] transition-all",
            add: "h-[43px] px-[12px] rounded-[12px] bg-[var(--modal-bg-hover)] text-[var(--modal-text-primary)] text-[13px] font-medium hover:bg-[var(--modal-border-primary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors flex items-center gap-1 cursor-pointer",
        },
    },
};

export function StringListEditor({ id, values, onChange, placeholder, variant = "default" }: {
    id: string; values: string[]; onChange: (v: string[]) => void; placeholder?: string;
    /** "prominent" matches TextInput's prominent variant (taller, larger text,
     *  thicker border, soft focus glow) — opt in per-field, same rationale as
     *  TextInput's variant prop above. */
    variant?: "default" | "prominent";
}) {
    const styles = STRING_LIST_STYLES[variant][useFormSurface()];
    const [draft, setDraft] = useState("");
    const add = () => { const t = draft.trim(); if (!t) return; onChange([...values, t]); setDraft(""); };
    return (
        <div className="flex flex-col gap-[6px]">
            {values.map((v, i) => (
                <div key={i} className="flex items-center gap-[6px]">
                    <span className={styles.chip}>{v}</span>
                    <button type="button" onClick={() => onChange(values.filter((_, j) => j !== i))}
                        className={styles.remove}>
                        <X className="w-[13px] h-[13px]" />
                    </button>
                </div>
            ))}
            <div className="flex gap-[6px]">
                <input id={id} type="text" value={draft} onChange={(e) => setDraft(e.target.value)}
                    onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); add(); } }}
                    placeholder={placeholder ?? "Add value…"}
                    autoCorrect="off" autoCapitalize="off" spellCheck={false}
                    className={styles.input}
                />
                <button type="button" onClick={add} disabled={!draft.trim()}
                    className={styles.add}>
                    <Plus className="w-[12px] h-[12px]" /> Add
                </button>
            </div>
        </div>
    );
}

const KV_EDITOR_STYLES: Record<"default" | "prominent", Record<FormSurface, { keyChip: string; valueChip: string; remove: string; input: string; add: string }>> = {
    default: {
        page: {
            keyChip: "w-[40%] h-[42px] px-[12px] rounded-[10px] bg-[var(--bg-hover)] text-[13px] font-mono text-[var(--text-primary)] flex items-center truncate",
            valueChip: "flex-1 h-[42px] px-[12px] rounded-[10px] bg-[var(--bg-hover)] text-[13px] font-mono text-[var(--text-secondary)] flex items-center truncate",
            remove: "w-[28px] h-[28px] rounded-[7px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--error-bg)] hover:text-[var(--error)] transition-colors cursor-pointer",
            input: "h-[42px] px-[12px] rounded-[10px] border border-[var(--border-secondary)] bg-[var(--bg-input)] text-[13px] font-mono text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] outline-none focus:border-[var(--accent)] focus:shadow-[0_0_0_1px_var(--accent)] transition-all",
            add: "h-[42px] px-[12px] rounded-[10px] bg-[var(--bg-hover)] text-[var(--text-primary)] text-[13px] font-medium hover:bg-[var(--border-primary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors flex items-center gap-1 cursor-pointer",
        },
        modal: {
            keyChip: "w-[40%] h-[42px] px-[12px] rounded-[10px] bg-[var(--modal-bg-hover)] text-[13px] font-mono text-[var(--modal-text-primary)] flex items-center truncate",
            valueChip: "flex-1 h-[42px] px-[12px] rounded-[10px] bg-[var(--modal-bg-hover)] text-[13px] font-mono text-[var(--modal-text-secondary)] flex items-center truncate",
            remove: "w-[28px] h-[28px] rounded-[7px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--error-bg)] hover:text-[var(--error)] transition-colors cursor-pointer",
            input: "h-[42px] px-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[13px] font-mono text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-all",
            add: "h-[42px] px-[12px] rounded-[10px] bg-[var(--modal-bg-hover)] text-[var(--modal-text-primary)] text-[13px] font-medium hover:bg-[var(--modal-border-primary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors flex items-center gap-1 cursor-pointer",
        },
    },
    prominent: {
        page: {
            keyChip: "w-[40%] h-[43px] px-[12px] rounded-[12px] bg-[var(--bg-hover)] text-[16px] font-mono text-[var(--text-primary)] flex items-center truncate",
            valueChip: "flex-1 h-[43px] px-[12px] rounded-[12px] bg-[var(--bg-hover)] text-[16px] font-mono text-[var(--text-secondary)] flex items-center truncate",
            remove: "w-[28px] h-[28px] rounded-[7px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--error-bg)] hover:text-[var(--error)] transition-colors cursor-pointer",
            input: "h-[43px] px-[12px] rounded-[12px] border-[1.5px] border-[color-mix(in_srgb,var(--border-secondary)_55%,var(--text-tertiary)_45%)] text-[16px] font-mono text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] outline-none focus:border-[var(--accent)] focus:shadow-[0_0_0_4px_color-mix(in_srgb,var(--accent)_22%,transparent)] transition-all",
            add: "h-[43px] px-[12px] rounded-[12px] bg-[var(--bg-hover)] text-[var(--text-primary)] text-[13px] font-medium hover:bg-[var(--border-primary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors flex items-center gap-1 cursor-pointer",
        },
        modal: {
            keyChip: "w-[40%] h-[43px] px-[12px] rounded-[12px] bg-[var(--modal-bg-hover)] text-[16px] font-mono text-[var(--modal-text-primary)] flex items-center truncate",
            valueChip: "flex-1 h-[43px] px-[12px] rounded-[12px] bg-[var(--modal-bg-hover)] text-[16px] font-mono text-[var(--modal-text-secondary)] flex items-center truncate",
            remove: "w-[28px] h-[28px] rounded-[7px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--error-bg)] hover:text-[var(--error)] transition-colors cursor-pointer",
            input: "h-[43px] px-[12px] rounded-[12px] border-[1.5px] border-[color-mix(in_srgb,var(--modal-border-secondary)_55%,var(--modal-text-tertiary)_45%)] text-[16px] font-mono text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_4px_color-mix(in_srgb,var(--modal-accent)_22%,transparent)] transition-all",
            add: "h-[43px] px-[12px] rounded-[12px] bg-[var(--modal-bg-hover)] text-[var(--modal-text-primary)] text-[13px] font-medium hover:bg-[var(--modal-border-primary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors flex items-center gap-1 cursor-pointer",
        },
    },
};

export function KVEditor({ values, onChange, keyPlaceholder, valuePlaceholder, variant = "default" }: {
    values: Record<string, string>; onChange: (v: Record<string, string>) => void;
    keyPlaceholder?: string; valuePlaceholder?: string;
    /** "prominent" matches TextInput's prominent variant — see StringListEditor's
     *  variant doc above for rationale. */
    variant?: "default" | "prominent";
}) {
    const styles = KV_EDITOR_STYLES[variant][useFormSurface()];
    const [dk, setDk] = useState(""); const [dv, setDv] = useState("");
    const add = () => { const k = dk.trim(); if (!k) return; onChange({ ...values, [k]: dv.trim() }); setDk(""); setDv(""); };
    return (
        <div className="flex flex-col gap-[6px]">
            {Object.entries(values).map(([k, v]) => (
                <div key={k} className="flex items-center gap-[6px]">
                    <span className={styles.keyChip}>{k}</span>
                    <span className={styles.valueChip}>{v}</span>
                    <button type="button" onClick={() => { const n = { ...values }; delete n[k]; onChange(n); }}
                        className={styles.remove}>
                        <X className="w-[13px] h-[13px]" />
                    </button>
                </div>
            ))}
            <div className="flex gap-[6px]">
                <input type="text" value={dk} onChange={(e) => setDk(e.target.value)} placeholder={keyPlaceholder ?? "Key"}
                    autoCorrect="off" autoCapitalize="off" spellCheck={false}
                    className={`w-[40%] ${styles.input}`}
                />
                <input type="text" value={dv} onChange={(e) => setDv(e.target.value)}
                    onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); add(); } }}
                    placeholder={valuePlaceholder ?? "Value"}
                    autoCorrect="off" autoCapitalize="off" spellCheck={false}
                    className={`flex-1 ${styles.input}`}
                />
                <button type="button" onClick={add} disabled={!dk.trim()}
                    className={styles.add}>
                    <Plus className="w-[12px] h-[12px]" /> Add
                </button>
            </div>
        </div>
    );
}

// ─── select-option constants ──────────────────────────────────────────────────

export const OUTPUT_FORMAT_OPTIONS = [
    { label: "Text", value: "Text" },
    { label: "Stream JSON", value: "StreamJson" },
    { label: "Stream JSONL", value: "StreamJsonl" },
    { label: "JSON", value: "Json" },
];

export const INPUT_MODE_OPTIONS = [
    { label: "Arg", value: "Arg" },
    { label: "Stdin", value: "Stdin" },
];

/** Options for the agent "Kind" selector — CLI process vs. in-process Native API runner. */
export const RUNNER_MODE_OPTIONS = [
    { label: "CLI", value: "cli" },
    { label: "Native (API)", value: "api" },
];

// ─── CLI templates + detection ────────────────────────────────────────────────

export interface CliTemplate {
    id: string;
    label: string;
    command: string;
    versionFlag: string;
}

export const CLI_TEMPLATES: CliTemplate[] = [
    { id: "claude", label: "Claude", command: "claude", versionFlag: "-v" },
    { id: "cursor", label: "Cursor", command: "cursor-agent", versionFlag: "-v" },
    { id: "codex", label: "Codex", command: "codex", versionFlag: "-V" },
    { id: "agy", label: "Antigravity", command: "agy", versionFlag: "--version" },
];

/**
 * Probe whether each configured CLI binary is on PATH. Returns a map of
 * template-id → `true | false | null` where `null` means "still probing".
 * Used by the template chip strip to show availability dots.
 */
export function useCliDetection() {
    const [availability, setAvailability] = useState<Record<string, boolean | null>>(() =>
        Object.fromEntries(CLI_TEMPLATES.map((t) => [t.id, null]))
    );

    useEffect(() => {
        let cancelled = false;
        const id = setTimeout(() => {
            for (const tpl of CLI_TEMPLATES) {
                invoke<boolean>("check_cli_available", {
                    command: tpl.command,
                    versionFlag: tpl.versionFlag,
                })
                    .then((available) => {
                        if (!cancelled) setAvailability((prev) => ({ ...prev, [tpl.id]: available }));
                    })
                    .catch(() => {
                        if (!cancelled) setAvailability((prev) => ({ ...prev, [tpl.id]: false }));
                    });
            }
        }, 0);
        return () => { cancelled = true; clearTimeout(id); };
    }, []);

    return availability;
}
