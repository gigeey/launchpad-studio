import {
    forwardRef,
    useCallback,
    useEffect,
    useImperativeHandle,
    useLayoutEffect,
    useRef,
    useState,
    type Dispatch,
    type SetStateAction,
} from "react";
import { AnimatePresence, motion } from "framer-motion";
import {
    BookUser,
    Check,
    ChevronRight,
    Copy,
    Eye,
    FolderOpen,
    Hash,
    Info as InfoIcon,
    Loader2,
    Mail,
    MessageSquare,
    MessageSquareText,
    RefreshCw,
    Send,
    Settings2,
    Slack,
    Trash2,
    X,
} from "lucide-react";
import { invoke } from "@tauri-apps/api/core";

import { agentAvatarColor } from "../../lib/agentColors";
import { useIsDark, useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { AGENT_TEMPLATES, DEFAULT_PERSONA } from "../../data/agentTemplates";
import {
    ApiError,
    createTelegramPairingCode,
    deleteDiscordChannel,
    deleteEmailChannel,
    deleteSlackChannel,
    deleteTelegramToken,
    getAgentChannels,
    getChannelSenders,
    getComposedPrompt,
    getSlackManifest,
    getTelegramStatus,
    setChannelSenders,
    setDiscordChannelSecret,
    setEmailChannelSecret,
    setSlackChannelSecret,
    setTelegramToken,
    testSlackConnection,
    unlinkTelegramChat,
    upsertDiscordChannel,
    upsertEmailChannel,
    upsertSlackChannel,
    type ChannelConnectionState,
    type ChannelStatus,
    type DiscordChannelConfig,
    type EmailChannelConfig,
    type SlackChannelConfig,
    type SlackConversationMode,
    type SlackTestConnectionReport,
    type TelegramStatus,
    type ThreadFollowMode,
} from "../../lib/api";
import { randomAgentEmoji } from "../../lib/randomAgentEmoji";
import type { AgentProfile, DelegateTarget, TelegramConfig } from "../../types/api";
import ConfirmDialog from "../ui/ConfirmDialog";
import { EmojiPicker } from "../ui/EmojiPicker";
import { AddressBookEditor } from "../profile/AddressBookEditor";
import { FormSelect, FormSurfaceProvider, Label, StringListEditor, TextInput } from "./formPrimitives";
import {
    CoordinatorConfigFields,
    coordinatorConfigFromProfile,
    maxTurnsValidationError,
    parseMaxTurns,
    type CoordinatorConfigFieldsValue,
} from "./CoordinatorConfigFields";

type TabId = "info" | "address_book" | "instructions" | "advanced" | "preview" | "channels";

const TABS: { id: TabId; label: string; icon: React.ComponentType<{ className?: string }> }[] = [
    { id: "info", label: "Info", icon: InfoIcon },
    { id: "advanced", label: "Advanced Settings", icon: Settings2 },
    { id: "instructions", label: "Instructions", icon: MessageSquareText },
    { id: "address_book", label: "Address Book", icon: BookUser },
    { id: "channels", label: "Channels", icon: MessageSquare },
    { id: "preview", label: "Prompt Preview", icon: Eye },
];

export interface AgentProfileModalProps {
    open: boolean;
    /** Pre-fill values for edit mode. Leave undefined for create mode. */
    initial?: AgentProfile;
    onClose: () => void;
    onSubmit: (profile: AgentProfile) => Promise<void>;
    onClone?: () => Promise<void>;
    onDelete?: (id: string) => Promise<void>;
}

export function AgentProfileModal({ open, initial, onClose, onSubmit, onClone, onDelete }: AgentProfileModalProps) {
    useEffect(() => {
        if (!open) return;
        const handler = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
        document.addEventListener("keydown", handler);
        return () => document.removeEventListener("keydown", handler);
    }, [open, onClose]);

    return (
        <AnimatePresence>
            {open && (
                <div className="fixed inset-0 z-[300] flex items-center justify-center">
                    <motion.div
                        initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}
                        transition={{ duration: 0.15 }}
                        className="absolute inset-0 bg-black/40"
                        onClick={onClose}
                    />
                    <motion.div
                        initial={{ opacity: 0, scale: 0.96 }} animate={{ opacity: 1, scale: 1 }} exit={{ opacity: 0, scale: 0.96 }}
                        transition={{ duration: 0.15, ease: "easeOut" }}
                        className="agent-profile-modal relative w-full max-w-[1000px] h-[720px] max-h-[88vh] rounded-[10px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)] flex flex-col"
                        style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
                    >
                        <AgentProfileFormBody initial={initial} onClose={onClose} onSubmit={onSubmit} onClone={onClone} onDelete={onDelete} />
                    </motion.div>
                </div>
            )}
        </AnimatePresence>
    );
}

function AgentProfileFormBody({ initial, onClose, onSubmit, onClone, onDelete }: { initial?: AgentProfile; onClose: () => void; onSubmit: (p: AgentProfile) => Promise<void>; onClone?: () => Promise<void>; onDelete?: (id: string) => Promise<void> }) {
    const isDark = useIsDark();
    const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);

    const isCreating = !initial;
    const [activeTab, setActiveTab] = useState<TabId>("info");

    // ── basic fields ──
    const [name, setName] = useState(initial?.name ?? "");
    const [agentId] = useState<string>(() => initial?.id ?? crypto.randomUUID());
    const [description, setDescription] = useState(initial?.description ?? "");
    // Editing an existing agent whose emoji is unset must seed the same "🤖"
    // fallback the rest of the app renders (see e.g. resolveAgent.ts's
    // FALLBACK_EMOJI) and that isDirty's baseline below compares against —
    // seeding a random emoji here instead made isDirty permanently true.
    // New agents still get a random pick so create mode doesn't default
    // every fresh agent to the same face.
    const [emoji, setEmoji] = useState(() => initial?.emoji ?? (isCreating ? randomAgentEmoji() : "🤖"));
    // New agents get no legacy system_prompt blob at all — persona/special_instructions
    // are the modern fields and the composer's own baseline sections cover the rest.
    // Edit mode still round-trips whatever an existing profile already has here (no UI
    // below edits it), so older un-migrated agents keep composing via the backend's
    // runtime persona/special_instructions fallback.
    const [systemPrompt, setSystemPrompt] = useState(() => initial?.system_prompt ?? "");
    const [persona, setPersona] = useState(() => initial?.persona ?? (isCreating ? DEFAULT_PERSONA : ""));
    const [specialInstructions, setSpecialInstructions] = useState(initial?.special_instructions ?? "");
    // Which of the Persona/Special Instructions boxes currently has focus, or null when
    // neither does (e.g. right after the modal/tab opens). Tracked in React rather than
    // pure CSS group-focus-within because the dimming below is *relative* — a box only
    // dims when its *sibling* has focus, not based on its own focus state alone — which
    // an unnamed-group selector can't express across two sibling sections.
    const [focusedSection, setFocusedSection] = useState<"persona" | "special" | null>(null);
    const [workingDir, setWorkingDir] = useState(initial?.working_dir ?? "");
    const [homeDir, setHomeDir] = useState(initial?.home_dir ?? "");

    // ── provider / advanced (bundled) ──
    const [advancedValue, setAdvancedValue] = useState<CoordinatorConfigFieldsValue>(() => coordinatorConfigFromProfile(initial));

    // ── delegate address book ──
    const initialDelegatesTo: DelegateTarget[] = initial?.delegates_to ?? [];
    const [delegatesTo, setDelegatesTo] = useState<DelegateTarget[]>(initialDelegatesTo);

    // ── Telegram bridge config (non-secret; the token itself never lives on this
    // draft — it's written directly via the dedicated token endpoints). Rides the
    // regular full-profile save so switching Save at the bottom doesn't silently
    // drop an already-configured bridge.
    const [telegramConfig, setTelegramConfig] = useState<TelegramConfig | null>(initial?.telegram ?? null);

    // ── Discord / Email / Slack channel saves (self-contained endpoints, no
    // profile-level draft) — each panel exposes an imperative save/isConfigured
    // pair the single primary Save button below drives directly. See
    // ChannelSaveHandle's doc comment.
    const discordSaveRef = useRef<ChannelSaveHandle>(null);
    const emailSaveRef = useRef<ChannelSaveHandle>(null);
    const slackSaveRef = useRef<ChannelSaveHandle>(null);
    // Ref.current changes don't trigger a re-render here, so each panel also
    // reports its own configured/not-configured transitions back up through
    // these — that's what lets the primary Save button react to a
    // channel-only edit (e.g. setting up Discord without touching any
    // profile field) instead of staying disabled until a profile field
    // happens to change too.
    const [discordConfigured, setDiscordConfigured] = useState(false);
    const [emailConfigured, setEmailConfigured] = useState(false);
    const [slackConfigured, setSlackConfigured] = useState(false);

    // ── prompt preview (Advanced tab) ──
    const [previewPrompt, setPreviewPrompt] = useState<string | null>(null);
    const [previewLoading, setPreviewLoading] = useState(false);
    const [previewError, setPreviewError] = useState<string | null>(null);

    const fetchPreview = useCallback(async () => {
        if (!initial?.id) return;
        setPreviewLoading(true);
        setPreviewError(null);
        try {
            const prompt = await getComposedPrompt(initial.id);
            setPreviewPrompt(prompt);
        } catch (err) {
            setPreviewError(err instanceof Error ? err.message : "Failed to fetch preview");
        } finally {
            setPreviewLoading(false);
        }
    }, [initial?.id]);

    // Load preview when switching to preview tab.
    useEffect(() => {
        if (activeTab === "preview" && previewPrompt === null && !previewLoading) {
            fetchPreview();
        }
    }, [activeTab, previewPrompt, previewLoading, fetchPreview]);

    // Debounced auto-refresh when persona/specialInstructions change.
    useEffect(() => {
        if (activeTab !== "preview") return;
        const timer = setTimeout(() => { fetchPreview(); }, 500);
        return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [persona, specialInstructions]);

    // ── dirty + submit state ──
    const [submitting, setSubmitting] = useState(false);
    const [cloning, setCloning] = useState(false);
    const [error, setError] = useState<string | null>(null);

    // ── delete state ──
    const [confirmDeleteOpen, setConfirmDeleteOpen] = useState(false);
    const [deleteError, setDeleteError] = useState<string | null>(null);

    const handleClone = async () => {
        if (!onClone || cloning) return;
        setError(null);
        setCloning(true);
        try {
            await onClone();
        } catch (err) {
            setError(err instanceof Error ? err.message : "Failed to clone agent");
        } finally {
            setCloning(false);
        }
    };

    const openDeleteConfirm = useCallback(() => {
        if (!onDelete || isCreating) return;
        setDeleteError(null);
        setConfirmDeleteOpen(true);
    }, [onDelete, isCreating]);

    const handleConfirmDelete = useCallback(async () => {
        if (!onDelete) return;
        setDeleteError(null);
        try {
            await onDelete(agentId);
            setConfirmDeleteOpen(false);
            onClose();
        } catch (err) {
            if (err instanceof ApiError && err.status === 409) {
                setDeleteError("This agent is the coordinator of a team and cannot be deleted. Reassign the coordinator before retrying.");
            } else {
                setDeleteError(err instanceof Error ? err.message : "Failed to delete agent");
            }
        }
    }, [agentId, onClose, onDelete]);

    const isDirty = isCreating ? true : (
        name !== (initial!.name ?? "") ||
        description !== (initial!.description ?? "") ||
        emoji !== (initial!.emoji ?? "🤖") ||
        systemPrompt !== (initial!.system_prompt ?? "") ||
        advancedValue.command !== (initial!.provider?.command ?? "echo") ||
        JSON.stringify(advancedValue.args) !== JSON.stringify(initial!.provider?.args ?? ["Hello from agent"]) ||
        advancedValue.outputFormat !== (initial!.provider?.output_format ?? "Text") ||
        advancedValue.inputMode !== (initial!.provider?.input_mode ?? "Arg") ||
        advancedValue.modelArg !== (initial!.provider?.model_arg ?? "") ||
        advancedValue.systemPromptArg !== (initial!.provider?.system_prompt_arg ?? "") ||
        advancedValue.sessionArg !== (initial!.provider?.session_arg ?? "") ||
        JSON.stringify(advancedValue.resumeArgs) !== JSON.stringify(initial!.provider?.resume_args ?? []) ||
        advancedValue.noOutputTimeoutMs !== String(initial!.provider?.no_output_timeout_ms ?? 30000) ||
        advancedValue.maxInstances !== String(initial!.max_instances ?? 1) ||
        advancedValue.timeoutSeconds !== String(initial!.timeout_seconds ?? 300) ||
        advancedValue.maxTurns !== (initial!.max_turns != null ? String(initial!.max_turns) : "") ||
        advancedValue.clearEnv !== (initial!.provider?.clear_env ?? false) ||
        JSON.stringify(advancedValue.env) !== JSON.stringify(initial!.env ?? {}) ||
        advancedValue.normalizer !== (initial!.provider?.normalizer ?? "") ||
        JSON.stringify(advancedValue.modelAliases) !== JSON.stringify(initial!.provider?.model_aliases ?? {}) ||
        advancedValue.model !== (initial!.model ?? "") ||
        workingDir !== (initial!.working_dir ?? "") ||
        homeDir !== (initial!.home_dir ?? "") ||
        advancedValue.selectedTemplate !== (initial!.template ?? null) ||
        advancedValue.runnerMode !== (initial!.runner_mode ?? "cli") ||
        advancedValue.nativeProvider !== (initial!.native_provider ?? "anthropic") ||
        JSON.stringify(delegatesTo) !== JSON.stringify(initialDelegatesTo) ||
        persona !== (initial!.persona ?? "") ||
        specialInstructions !== (initial!.special_instructions ?? "") ||
        JSON.stringify(telegramConfig) !== JSON.stringify(initial!.telegram ?? null)
    );

    // Whether the primary Save button can actually do something: either a
    // profile field changed, or a self-contained channel tab (Discord/Email/
    // Slack) has something worth persisting through its own endpoint. Reset
    // stays tied to isDirty alone — it only knows how to reset profile
    // fields, not the channel tabs' own local state.
    const maxTurnsError = maxTurnsValidationError(advancedValue.maxTurns);
    const canSave = (isDirty || discordConfigured || emailConfigured || slackConfigured) && !maxTurnsError;

    const handleReset = useCallback(() => {
        if (!initial) return;
        setName(initial.name ?? "");
        setDescription(initial.description ?? "");
        setEmoji(initial.emoji ?? "🤖");
        setSystemPrompt(initial.system_prompt ?? "");
        setPersona(initial.persona ?? "");
        setSpecialInstructions(initial.special_instructions ?? "");
        setWorkingDir(initial.working_dir ?? "");
        setHomeDir(initial.home_dir ?? "");
        setAdvancedValue(coordinatorConfigFromProfile(initial));
        setDelegatesTo(initial.delegates_to ?? []);
        setTelegramConfig(initial.telegram ?? null);
        setError(null);
    }, [initial]);

    // ── apply template ──
    const applyTemplate = useCallback((templateId: string) => {
        const tpl = AGENT_TEMPLATES[templateId];
        if (!tpl) return;
        setAdvancedValue((prev) => ({
            ...prev,
            command: tpl.provider.command,
            args: [...tpl.provider.args],
            outputFormat: tpl.provider.output_format,
            inputMode: tpl.provider.input_mode,
            normalizer: tpl.provider.normalizer ?? "",
            modelArg: tpl.provider.model_arg ?? "",
            systemPromptArg: tpl.provider.system_prompt_arg ?? "",
            sessionArg: tpl.provider.session_arg ?? "",
            resumeArgs: [...tpl.provider.resume_args],
            clearEnv: tpl.provider.clear_env,
            noOutputTimeoutMs: String(tpl.provider.no_output_timeout_ms),
            maxInstances: String(tpl.max_instances),
            timeoutSeconds: String(tpl.timeout_seconds),
            modelAliases: {},
            model: "",
            customModelMode: false,
        }));
    }, []);

    // Seed prevTemplateRef with the persisted template so re-mounts don't
    // overwrite custom field edits on first render when restoring state.
    const prevTemplateRef = useRef<string | null>(initial?.template ?? null);
    useEffect(() => {
        if (advancedValue.selectedTemplate && advancedValue.selectedTemplate !== prevTemplateRef.current) {
            applyTemplate(advancedValue.selectedTemplate);
        }
        prevTemplateRef.current = advancedValue.selectedTemplate;
    }, [advancedValue.selectedTemplate, applyTemplate]);

    // ── submit ──
    // Channel configs (Discord/Email/Slack) are saved through their own
    // dedicated endpoints — not part of the AgentProfile payload below — so
    // they're applied first, ahead of the profile save. That ordering also
    // means a channel failure keeps the modal open with a clear error
    // instead of silently discarding it behind onSubmit's close-and-navigate.
    // Only channels the user actually configured are touched; agentId is
    // already persisted by the time any of these tabs are usable (they're
    // gated on !isCreating), so no channel needs the profile saved first.
    const saveConfiguredChannels = async (): Promise<string[]> => {
        if (isCreating) return [];
        const channels: { label: string; ref: React.RefObject<ChannelSaveHandle | null> }[] = [
            { label: "Discord", ref: discordSaveRef },
            { label: "Email", ref: emailSaveRef },
            { label: "Slack", ref: slackSaveRef },
        ];
        const failures: string[] = [];
        for (const { label, ref } of channels) {
            const handle = ref.current;
            if (!handle || !handle.isConfigured()) continue;
            const result = await handle.save();
            if (!result.ok) failures.push(`${label}: ${result.error}`);
        }
        return failures;
    };

    const handleSubmit = async (e: React.FormEvent) => {
        e.preventDefault();
        if (!name.trim()) return;
        setError(null); setSubmitting(true);

        const channelFailures = await saveConfiguredChannels();
        if (channelFailures.length > 0) {
            setError(`Couldn't save channel settings — ${channelFailures.join("; ")}`);
            setSubmitting(false);
            return;
        }

        const profile: AgentProfile = {
            id: agentId,
            name: name.trim(),
            description: description.trim(),
            emoji,
            provider: {
                type: "Cli",
                command: advancedValue.command.trim() || "echo",
                args: advancedValue.args,
                output_format: advancedValue.outputFormat,
                input_mode: advancedValue.inputMode,
                normalizer: advancedValue.normalizer.trim() || null,
                model_arg: advancedValue.modelArg.trim() || null,
                model_aliases: advancedValue.modelAliases,
                system_prompt_arg: advancedValue.systemPromptArg.trim() || null,
                session_arg: advancedValue.sessionArg.trim() || null,
                resume_args: advancedValue.resumeArgs,
                session_id_fields: [],
                clear_env: advancedValue.clearEnv,
                no_output_timeout_ms: parseInt(advancedValue.noOutputTimeoutMs) || 30000,
            },
            model: advancedValue.model.trim() || null,
            skills: [],
            system_prompt: systemPrompt.trim() || null,
            persona: persona.trim() || null,
            special_instructions: specialInstructions.trim() || null,
            tools: null,
            env: advancedValue.env,
            max_instances: parseInt(advancedValue.maxInstances) || 1,
            timeout_seconds: parseInt(advancedValue.timeoutSeconds) || 300,
            max_turns: parseMaxTurns(advancedValue.maxTurns),
            working_dir: workingDir.trim() || null,
            home_dir: homeDir.trim() || null,
            serialize: true,
            workflows: initial?.workflows,
            template: advancedValue.selectedTemplate ?? null,
            runner_mode: advancedValue.runnerMode,
            // native_provider only matters for API mode. Persist it on
            // CLI-mode profiles too so a future runner-mode flip doesn't
            // silently regress to the default — the backend ignores the
            // field when runner_mode is `cli`.
            native_provider: advancedValue.nativeProvider,
            delegates_to: delegatesTo.length > 0 ? delegatesTo : undefined,
            telegram: telegramConfig,
        };
        try {
            await onSubmit(profile);
        } catch (err) {
            setError(err instanceof Error ? err.message : "Something went wrong");
            setSubmitting(false);
        }
    };

    const avatarBg = agentAvatarColor(name || initial?.name || "x", isDark);

    return (
        <FormSurfaceProvider surface="modal">
        <form onSubmit={handleSubmit} className="flex flex-col flex-1 min-h-0 min-w-0 overflow-hidden bg-[var(--modal-bg)]">
            {/* ── Body (sidebar + content) ── */}
            <div className="flex flex-1 min-h-0 min-w-0 overflow-hidden">
                {/* Sidebar cutout */}
                <div className="flex-shrink-0  pr-0 flex">
                    <nav className="w-[290px] flex-shrink-0 flex flex-col p-[26px] bg-[var(--modal-bg)] rounded-[0px]">
                        {/* Top profile area */}
                        <div className="relative w-full aspect-square rounded-[16px] flex items-center justify-center mb-[20px] shadow-[inset_0_0_16px_4px_rgba(0,0,0,0.55)] overflow-hidden"
                            style={{ background: `linear-gradient(135deg, ${avatarBg} 0%, ${avatarBg}cc 60%, ${avatarBg}99 100%)` }}>
                            <div className="relative">
                                <EmojiPicker
                                    value={emoji}
                                    onChange={setEmoji}
                                    triggerClassName={`relative w-[100px] h-[100px] ${circularAvatars ? "rounded-full" : "rounded-[14px]"} bg-white/25 border-2 border-white/40 flex items-center justify-center text-[56px] hover:border-white/80 transition-all cursor-pointer select-none shadow-md backdrop-blur-sm`}
                                />
                            </div>
                            {isCreating && (
                                <div className="absolute inset-x-0 bottom-0 px-[12px] py-[6px] bg-black text-center pointer-events-none">
                                    <span className="text-white text-[12px] font-semibold tracking-wide">New Here</span>
                                </div>
                            )}
                        </div>

                        <div className="flex flex-col gap-[4px] mt-[4px]">
                            {TABS.map((t) => {
                                const active = activeTab === t.id;
                                // The sketch does not show icons so we could omit them or keep them since they complement the UI nicely.
                                return (
                                    <button
                                        key={t.id}
                                        type="button"
                                        onClick={() => setActiveTab(t.id)}
                                        className={`flex items-center gap-[10px] px-[12px] py-[7px] rounded-[8px] text-left text-[15px] font-medium transition-colors cursor-pointer ${active
                                            ? "bg-[#1164A3] text-white"
                                            : "text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)]"
                                            }`}
                                    >
                                        <span>{t.label}</span>
                                    </button>
                                );
                            })}
                        </div>

                        {!isCreating && (
                            <div className="mt-auto flex flex-col gap-[4px] pt-[12px]">
                                <button
                                    type="button"
                                    onClick={handleClone}
                                    disabled={!onClone || cloning || submitting}
                                    className="flex items-center gap-[10px] px-[12px] py-[9px] rounded-[12px] text-left text-[14px] font-medium text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                                >
                                    {cloning ? <Loader2 className="w-[15px] h-[15px] animate-spin" /> : <Copy className="w-[15px] h-[15px]" />}
                                    <span>{cloning ? "Cloning…" : "Clone"}</span>
                                </button>
                                <button
                                    type="button"
                                    onClick={openDeleteConfirm}
                                    disabled={!onDelete || confirmDeleteOpen || submitting || cloning}
                                    className="flex items-center gap-[10px] px-[12px] py-[9px] rounded-[12px] text-left text-[14px] font-medium text-[#E01E5A] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                                >
                                    <Trash2 className="w-[15px] h-[15px]" />
                                    <span>Delete</span>
                                </button>
                            </div>
                        )}
                    </nav>
                </div>

                {/* Content */}
                <div className="flex-1 flex flex-col min-h-0 min-w-0">
                    <div className="flex items-center justify-between px-[28px] py-[22px] pb-[36px]">
                        <h1 className="text-[28px] font-bold tracking-tight text-[var(--modal-text-primary)]">
                            {TABS.find(t => t.id === activeTab)?.label}
                        </h1>
                        <button
                            type="button"
                            onClick={onClose}
                            className="w-[32px] h-[32px] rounded-[8px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
                            aria-label="Close"
                        >
                            <X className="w-[20px] h-[20px]" />
                        </button>
                    </div>

                    {/* pt-[8px]: the instructions panel's focus-within halo (shadow-[0_0_0_4px_...])
                        needs breathing room above it — with no top padding here, the box sits flush
                        against this scroll container's own top edge and overflow-y-auto clips any part
                        of the halo that would render above y=0, cutting the top of the ring off. */}
                    <div className={`flex-1 overflow-y-auto px-[28px] pt-[8px] ${activeTab === "instructions" ? "pb-[8px]" : "pb-[22px]"}`}>
                        {activeTab === "info" && (
                            <div className="flex flex-col gap-[18px]">
                                <div>
                                    <Label htmlFor="ae-name">Name <span className="text-red-400">*</span></Label>
                                    <TextInput id="ae-name" value={name} onChange={setName} placeholder="e.g. Research Assistant" required variant="prominent" />
                                </div>
                                <div>
                                    <Label htmlFor="ae-desc">Description</Label>
                                    <TextInput id="ae-desc" value={description} onChange={setDescription} placeholder="What does this agent do?" variant="prominent" />
                                </div>
                                <div>
                                    <Label htmlFor="ae-id">Agent ID</Label>
                                    <TextInput id="ae-id" value={agentId} onChange={() => { }} monospace disabled variant="prominent" />
                                    <p className="mt-[6px] text-[11px] text-[var(--modal-text-tertiary)]">ID cannot be changed after creation.</p>
                                </div>
                                <div>
                                    <Label htmlFor="ae-workdir">Working Directory</Label>
                                    <DirInput id="ae-workdir" value={workingDir} onChange={setWorkingDir} placeholder="/path/to/project" />
                                    <p className="mt-[6px] text-[11px] text-[var(--modal-text-tertiary)]">If set, the agent will run from this directory.</p>
                                </div>
                                <div>
                                    <Label htmlFor="ae-homedir">Home Directory</Label>
                                    <DirInput id="ae-homedir" value={homeDir} onChange={setHomeDir} placeholder="Optional: custom path for skills, rules, instructions" />
                                    <p className="mt-[6px] text-[11px] text-[var(--modal-text-tertiary)]">Where this agent stores skills, rules, and instructions. Leave empty to use the default.</p>
                                </div>
                            </div>
                        )}

                        {activeTab === "address_book" && (
                            <div className="max-w-[720px] flex flex-col gap-[12px] -mt-[16px]">
                                {/* The tab already has its own H1 ("Address Book") above the scroll
                                    container, so this is description-only — no repeated title.
                                    -mt pulls it up closer to that H1 (the shared header padding
                                    otherwise leaves a large gap before this tab's content starts). */}
                                <p className="text-[13px] text-[var(--modal-text-secondary)] mb-[4px]">
                                    Agents this agent can hand work to — both as subagents via the Delegate tool and
                                    as owners of todo-list tasks. Self-delegation is not permitted.
                                </p>
                                <AddressBookEditor
                                    profileId={agentId}
                                    value={delegatesTo}
                                    onChange={setDelegatesTo}
                                />
                            </div>
                        )}

                        {activeTab === "instructions" && (
                            <div className="max-w-[720px]">
                                {/* Persona and Special Instructions are two stacked sections inside ONE
                                    outer-bordered panel — a single internal divider line separates them,
                                    so the pair reads as one grouped box, not two. Each section's title is
                                    a small self-bordered chip positioned with clear top/left margin fully
                                    INSIDE its own section — it never straddles the outer border or the
                                    divider, so neither line ever cuts through it. At-rest border color
                                    matches the other "prominent" inputs in this modal — Name/Description
                                    on the Info tab and DirInput — which use a mixed tone
                                    (color-mix of --modal-border-secondary/--modal-text-tertiary), not the
                                    plain --modal-border-secondary used by plainer fields like the preview
                                    textarea. */}
                                <div className="rounded-[10px] border border-[color-mix(in_srgb,var(--modal-border-secondary)_55%,var(--modal-text-tertiary)_45%)] overflow-hidden focus-within:border-[var(--modal-accent)] focus-within:shadow-[0_0_0_4px_color-mix(in_srgb,var(--modal-accent)_22%,transparent)] transition-all">
                                    <div
                                        className={`relative group border-b transition-colors ${focusedSection ? "border-[var(--modal-accent)]" : "border-[color-mix(in_srgb,var(--modal-border-secondary)_55%,var(--modal-text-tertiary)_45%)]"}`}
                                        onMouseDown={focusSectionOnMouseDown}
                                        onFocus={() => setFocusedSection("persona")}
                                        onBlur={() => setFocusedSection((s) => (s === "persona" ? null : s))}
                                    >
                                        <FloatingBoxLabel htmlFor="ae-persona">Persona</FloatingBoxLabel>
                                        <div className="px-[16px] pt-[50px] pb-[14px]">
                                            <AutoGrowTextarea
                                                id="ae-persona"
                                                value={persona}
                                                onChange={setPersona}
                                                placeholder="Describe the identity, voice, expertise, and communication style of this agent..."
                                                dim={focusedSection !== null && focusedSection !== "persona"}
                                            />
                                        </div>
                                    </div>
                                    <div
                                        className="relative group"
                                        onMouseDown={focusSectionOnMouseDown}
                                        onFocus={() => setFocusedSection("special")}
                                        onBlur={() => setFocusedSection((s) => (s === "special" ? null : s))}
                                    >
                                        <FloatingBoxLabel htmlFor="ae-special-instructions">Special Instructions</FloatingBoxLabel>
                                        <div className="px-[16px] pt-[50px] pb-[14px]">
                                            <AutoGrowTextarea
                                                id="ae-special-instructions"
                                                value={specialInstructions}
                                                onChange={setSpecialInstructions}
                                                placeholder="Add behavior rules, do's and don'ts, project-specific guidelines, or workflow preferences..."
                                                dim={focusedSection !== null && focusedSection !== "special"}
                                            />
                                        </div>
                                    </div>
                                </div>
                            </div>
                        )}

                        {activeTab === "advanced" && (
                            <CoordinatorConfigFields
                                value={advancedValue}
                                onChange={setAdvancedValue}
                                lockRunnerMode={!isCreating}
                            />
                        )}

                        {activeTab === "channels" && (
                            <div className="max-w-[560px]">
                                <ChannelsTabPanel
                                    agentId={agentId}
                                    isCreating={isCreating}
                                    telegramConfig={telegramConfig}
                                    onTelegramConfigChange={setTelegramConfig}
                                    discordSaveRef={discordSaveRef}
                                    emailSaveRef={emailSaveRef}
                                    slackSaveRef={slackSaveRef}
                                    onDiscordConfiguredChange={setDiscordConfigured}
                                    onEmailConfiguredChange={setEmailConfigured}
                                    onSlackConfiguredChange={setSlackConfigured}
                                />
                            </div>
                        )}

                        {activeTab === "preview" && (
                            <div className="flex flex-col gap-[12px] h-full">
                                <div className="flex items-center justify-between">
                                    <p className="text-[12px] text-[var(--modal-text-tertiary)]">Preview (empty session state)</p>
                                    <button
                                        type="button"
                                        onClick={fetchPreview}
                                        disabled={previewLoading || !initial?.id}
                                        className="flex items-center gap-[6px] px-[10px] py-[5px] rounded-[8px] text-[12px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] disabled:opacity-50 disabled:cursor-not-allowed transition-colors cursor-pointer"
                                    >
                                        <RefreshCw className={`w-[13px] h-[13px] ${previewLoading ? "animate-spin" : ""}`} />
                                        Refresh
                                    </button>
                                </div>
                                {previewError && (
                                    <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                                        {previewError}
                                    </div>
                                )}
                                {!initial?.id && (
                                    <p className="text-[13px] text-[var(--modal-text-tertiary)] italic">Save the agent first to preview the composed prompt.</p>
                                )}
                                {initial?.id && (
                                    <textarea
                                        readOnly
                                        value={previewPrompt ?? ""}
                                        className="flex-1 min-h-[400px] px-[12px] py-[10px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[12px] font-mono text-[var(--modal-text-primary)] outline-none resize-none"
                                        style={{ opacity: previewLoading ? 0.5 : 1 }}
                                    />
                                )}
                            </div>
                        )}

                    </div>

                    {/* ── Footer ── */}
                    <div className="flex-shrink-0 flex items-center justify-end gap-[10px] px-[24px] py-[14px] border-t-0 border-[var(--modal-border-secondary)] bg-[var(--modal-bg)]">
                        {error && (
                            <div className="flex-1 px-[12px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)] mr-[12px]">{error}</div>
                        )}
                        {!isCreating && isDirty && (
                            <button type="button" onClick={handleReset}
                                className="h-[36px] px-[14px] rounded-[8px] text-[13px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer">
                                Reset
                            </button>
                        )}
                        <button
                            type="submit"
                            disabled={submitting || !name.trim() || !canSave}
                            className={`h-[42px] px-[18px] rounded-[8px] text-[13px] font-semibold transition-colors flex items-center gap-[8px] ${canSave && name.trim()
                                ? "bg-[#006E51] hover:bg-[#005a43] active:bg-[#004d39] text-white cursor-pointer"
                                : "bg-[var(--modal-bg-hover)] text-[var(--modal-text-tertiary)] cursor-not-allowed"
                                }`}
                        >
                            {submitting && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
                            {submitting ? (isCreating ? "Creating…" : "Saving…") : (isCreating ? "Create Agent" : "Save Changes")}
                        </button>
                    </div>
                </div>
            </div>

            <ConfirmDialog
                open={confirmDeleteOpen}
                title="Delete agent?"
                destructive
                confirmLabel="Delete"
                cancelLabel="Cancel"
                onCancel={() => { setConfirmDeleteOpen(false); setDeleteError(null); }}
                onConfirm={handleConfirmDelete}
                message={
                    <div className="flex flex-col gap-[8px]">
                        <p>
                            This will permanently delete the agent profile and its home directory.
                            Past messages from this agent remain in threads, but will render as
                            <span className="font-mono"> id + 🤖</span> going forward.
                        </p>
                    </div>
                }
            >
                <div className="flex flex-col gap-[10px]">
                    {deleteError && (
                        <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                            {deleteError}
                        </div>
                    )}
                </div>
            </ConfirmDialog>
        </form>
        </FormSurfaceProvider>
    );
}

// ─── Channels tab ───────────────────────────────────────────────────────────────

/** Imperative surface a self-contained channel tab (Discord/Email/Slack) exposes
 *  to the modal's single primary Save button, so that button can persist
 *  whichever channels the user actually configured without lifting each
 *  channel's whole field set into the parent draft. `save` reuses the exact
 *  same request the panel's own (removed) save control used to fire, and
 *  still updates the panel's own inline error state — so an error is visible
 *  if the user is looking at that tab, and is also reported back to the
 *  caller to fold into the modal-level error banner. */
export interface ChannelSaveHandle {
    isConfigured: () => boolean;
    save: () => Promise<{ ok: true } | { ok: false; error: string }>;
}

const CHANNEL_SUB_TABS: { id: "telegram" | "discord" | "email" | "slack"; label: string; icon: React.ComponentType<{ className?: string }> }[] = [
    { id: "telegram", label: "Telegram", icon: Send },
    { id: "discord", label: "Discord", icon: Hash },
    { id: "email", label: "Email", icon: Mail },
    { id: "slack", label: "Slack", icon: Slack },
];

const CHANNEL_CONNECTION_LABEL: Record<ChannelConnectionState, string> = {
    connected: "Connected",
    reconnecting: "Reconnecting…",
    disconnected: "Disconnected",
    "not-holding-lease": "Held by another process",
};

/** `not-holding-lease` is the state most likely to read as an error if it
 *  isn't explained: it means a *different* backend process currently owns
 *  this connection (e.g. a second worktree pointed at the same data
 *  directory), not that anything is broken. */
const CHANNEL_CONNECTION_HINT: Record<ChannelConnectionState, string> = {
    connected: "This backend process has a live connection to this channel.",
    reconnecting: "This backend process is attempting to (re)establish the connection.",
    disconnected: "No backend process is currently running this channel binding.",
    "not-holding-lease":
        "Another backend process currently owns this connection right now — for example, a second worktree pointed at the same data directory. This isn't an error: only one process may hold a channel connection at a time, and this is the one that yielded.",
};

/** Live connection-state indicator for a channel binding — deliberately
 *  separate from the "Enabled"/"Disabled" badge next to
 *  it, which only reflects saved config. An enabled binding can still be
 *  disconnected, reconnecting, or held by another backend process, and
 *  conflating those readings with "enabled" is exactly the confusing
 *  silence this indicator replaces. Renders nothing while the state hasn't
 *  loaded yet. */
function ChannelConnectionBadge({ state }: { state: ChannelConnectionState | null | undefined }) {
    if (!state) return null;
    const colorClass =
        state === "connected"
            ? "text-green-600"
            : state === "reconnecting"
                ? "text-amber-500"
                : state === "not-holding-lease"
                    ? "text-[var(--modal-accent)]"
                    : "text-[var(--modal-text-tertiary)]";
    const Icon =
        state === "connected" ? Check : state === "reconnecting" ? RefreshCw : state === "not-holding-lease" ? InfoIcon : X;
    return (
        <span
            className={`inline-flex items-center gap-[5px] font-medium ${colorClass}`}
            title={CHANNEL_CONNECTION_HINT[state]}
        >
            <Icon className={`w-[12px] h-[12px] ${state === "reconnecting" ? "animate-spin" : ""}`} />
            {CHANNEL_CONNECTION_LABEL[state]}
        </span>
    );
}

/** Groups the three channel setup panels behind one "Channels" tab with a
 *  horizontal sub-tab bar, defaulting to Telegram. `telegramConfig`/
 *  `onTelegramConfigChange` are threaded through to [`TelegramTabPanel`]
 *  unchanged — that panel's enable toggle rides the main-form draft (see its
 *  own doc comment), so this wrapper can't drop them without breaking Save. */
export function ChannelsTabPanel({
    agentId,
    isCreating,
    telegramConfig,
    onTelegramConfigChange,
    discordSaveRef,
    emailSaveRef,
    slackSaveRef,
    onDiscordConfiguredChange,
    onEmailConfiguredChange,
    onSlackConfiguredChange,
}: {
    agentId: string;
    isCreating: boolean;
    telegramConfig: TelegramConfig | null;
    onTelegramConfigChange: Dispatch<SetStateAction<TelegramConfig | null>>;
    discordSaveRef?: React.Ref<ChannelSaveHandle>;
    emailSaveRef?: React.Ref<ChannelSaveHandle>;
    slackSaveRef?: React.Ref<ChannelSaveHandle>;
    onDiscordConfiguredChange?: (configured: boolean) => void;
    onEmailConfiguredChange?: (configured: boolean) => void;
    onSlackConfiguredChange?: (configured: boolean) => void;
}) {
    const [activeChannel, setActiveChannel] = useState<"telegram" | "discord" | "email" | "slack">("telegram");
    // Once a sub-tab has been visited, keep its panel mounted (just hidden)
    // instead of unmounting it on switch — Discord/Email/Slack each hold
    // their own local, unsaved field state, and the single primary Save
    // button at the bottom of the modal needs that state to still be alive
    // (and its imperative ref still attached) even after the user has since
    // clicked over to a different channel sub-tab to configure that one too.
    const [visited, setVisited] = useState<Set<"telegram" | "discord" | "email" | "slack">>(() => new Set(["telegram"]));

    const selectChannel = (id: "telegram" | "discord" | "email" | "slack") => {
        setActiveChannel(id);
        setVisited((prev) => (prev.has(id) ? prev : new Set(prev).add(id)));
    };

    return (
        <div className="flex flex-col gap-[16px]">
            <div className="inline-flex items-center gap-[4px] self-start rounded-[10px] border border-[color-mix(in_srgb,var(--modal-border-secondary)_55%,var(--modal-text-tertiary)_45%)] p-[4px]">
                {CHANNEL_SUB_TABS.map((t) => {
                    const active = activeChannel === t.id;
                    const Icon = t.icon;
                    return (
                        <button
                            key={t.id}
                            type="button"
                            onClick={() => selectChannel(t.id)}
                            className={`flex items-center gap-[6px] px-[12px] py-[6px] rounded-[8px] text-[13px] font-medium transition-colors cursor-pointer ${active
                                ? "bg-[#1164A3] text-white shadow-sm"
                                : "text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)]"
                                }`}
                        >
                            <Icon className="w-[13px] h-[13px]" />
                            <span>{t.label}</span>
                        </button>
                    );
                })}
            </div>

            {visited.has("telegram") && (
                <div className={activeChannel === "telegram" ? undefined : "hidden"}>
                    <TelegramTabPanel
                        agentId={agentId}
                        isCreating={isCreating}
                        config={telegramConfig}
                        onConfigChange={onTelegramConfigChange}
                    />
                </div>
            )}
            {visited.has("discord") && (
                <div className={activeChannel === "discord" ? undefined : "hidden"}>
                    <DiscordTabPanel
                        ref={discordSaveRef}
                        agentId={agentId}
                        isCreating={isCreating}
                        onConfiguredChange={onDiscordConfiguredChange}
                    />
                </div>
            )}
            {visited.has("email") && (
                <div className={activeChannel === "email" ? undefined : "hidden"}>
                    <EmailTabPanel
                        ref={emailSaveRef}
                        agentId={agentId}
                        isCreating={isCreating}
                        onConfiguredChange={onEmailConfiguredChange}
                    />
                </div>
            )}
            {visited.has("slack") && (
                <div className={activeChannel === "slack" ? undefined : "hidden"}>
                    <SlackTabPanel
                        ref={slackSaveRef}
                        agentId={agentId}
                        isCreating={isCreating}
                        onConfiguredChange={onSlackConfiguredChange}
                    />
                </div>
            )}
        </div>
    );
}

// ─── Telegram tab ──────────────────────────────────────────────────────────────

/** Setup surface for the per-agent Telegram bridge. The bot token is write-only —
 *  it lives only in the input while the user types it, is sent once via the
 *  dedicated token endpoints, and is never stored or re-rendered afterward.
 *  `@bot_username`/`has_token`/`linked` come back from `GET …/telegram/status`,
 *  which never echoes the token itself.
 *
 *  `config`/`onConfigChange` carry the non-secret `AgentProfile.telegram` draft
 *  (enable flag, thread mode, server-owned bridge thread id) that rides the
 *  regular full-profile Save at the bottom of the modal — kept in sync with
 *  `status` here so an unrelated edit-and-save elsewhere in the form can't wipe
 *  an already-configured bridge. Gated on `isCreating` because the token
 *  endpoints operate on a persisted agent profile (404 on an id the backend
 *  hasn't seen yet), same constraint as the Prompt Preview tab. */
export function TelegramTabPanel({
    agentId,
    isCreating,
    config,
    onConfigChange,
}: {
    agentId: string;
    isCreating: boolean;
    config?: TelegramConfig | null;
    onConfigChange?: Dispatch<SetStateAction<TelegramConfig | null>>;
}) {
    const [status, setStatus] = useState<TelegramStatus | null>(null);
    const [statusLoading, setStatusLoading] = useState(false);
    const [statusError, setStatusError] = useState<string | null>(null);
    const [connectionState, setConnectionState] = useState<ChannelConnectionState | null>(null);

    const [showForm, setShowForm] = useState(false);
    const [tokenInput, setTokenInput] = useState("");
    const [saving, setSaving] = useState(false);
    const [saveError, setSaveError] = useState<string | null>(null);

    const [removing, setRemoving] = useState(false);
    const [removeError, setRemoveError] = useState<string | null>(null);

    const [pairingLoading, setPairingLoading] = useState(false);
    const [pairingError, setPairingError] = useState<string | null>(null);
    const [codeCopied, setCodeCopied] = useState(false);
    const [howToConnectOpen, setHowToConnectOpen] = useState(false);

    const [unlinkingChatId, setUnlinkingChatId] = useState<number | null>(null);
    const [unlinkError, setUnlinkError] = useState<string | null>(null);

    // Folds a fresh status read into the draft config, preserving whatever
    // server-owned fields (bridge_thread_id, allowed_chat_ids) the draft
    // already knew about — this tab never edits those directly.
    const syncConfigFromStatus = useCallback((s: TelegramStatus) => {
        onConfigChange?.((prev) => ({
            enabled: s.enabled,
            bot_username: s.bot_username,
            thread_mode: "dedicated",
            bridge_thread_id: prev?.bridge_thread_id ?? null,
            allowed_chat_ids: prev?.allowed_chat_ids ?? [],
        }));
    }, [onConfigChange]);

    useEffect(() => {
        if (isCreating) return;
        let cancelled = false;
        setStatusLoading(true);
        setStatusError(null);
        getTelegramStatus(agentId)
            .then((s) => {
                if (cancelled) return;
                setStatus(s);
                setShowForm(!s.has_token);
                syncConfigFromStatus(s);
            })
            .catch((err) => {
                if (cancelled) return;
                setStatusError(err instanceof Error ? err.message : "Failed to load Telegram status");
            })
            .finally(() => {
                if (!cancelled) setStatusLoading(false);
            });
        return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [agentId, isCreating]);

    // Telegram's own status endpoint (above) doesn't carry connection state —
    // that lives on the generic per-binding surface (`GET …/channels`), the
    // same one Discord's and Email's tabs already read their status from.
    // A separate fetch keeps this tab's richer Telegram-specific status
    // (bot username, linked chats, pairing) on its own dedicated endpoint.
    useEffect(() => {
        if (isCreating) return;
        let cancelled = false;
        getAgentChannels(agentId)
            .then((channels) => {
                if (cancelled) return;
                setConnectionState(channels.find((c) => c.kind === "telegram")?.connection_state ?? null);
            })
            .catch(() => {
                // Best-effort: this tab's primary status (above) already
                // surfaces a load error, so a failure here just leaves the
                // connection badge blank rather than duplicating an error.
            });
        return () => { cancelled = true; };
    }, [agentId, isCreating]);

    const handleSave = async () => {
        const trimmed = tokenInput.trim();
        if (!trimmed) return;
        setSaving(true);
        setSaveError(null);
        try {
            const result = await setTelegramToken(agentId, trimmed);
            const next: TelegramStatus = { has_token: true, bot_username: result.bot_username, enabled: true, linked: status?.linked ?? false };
            setStatus(next);
            syncConfigFromStatus(next);
            setTokenInput("");
            setShowForm(false);
        } catch (err) {
            setSaveError(err instanceof Error ? err.message : "Failed to save token");
        } finally {
            setSaving(false);
        }
    };

    const handleRemove = async () => {
        setRemoving(true);
        setRemoveError(null);
        try {
            await deleteTelegramToken(agentId);
            const next: TelegramStatus = { has_token: false, bot_username: null, enabled: false, linked: false };
            setStatus(next);
            syncConfigFromStatus(next);
            setShowForm(true);
        } catch (err) {
            setRemoveError(err instanceof Error ? err.message : "Failed to remove token");
        } finally {
            setRemoving(false);
        }
    };

    const handleGeneratePairingCode = async () => {
        setPairingLoading(true);
        setPairingError(null);
        try {
            await createTelegramPairingCode(agentId);
            setStatus(await getTelegramStatus(agentId));
        } catch (err) {
            setPairingError(err instanceof Error ? err.message : "Failed to generate pairing code");
        } finally {
            setPairingLoading(false);
        }
    };

    const handleCopyCode = async (code: string) => {
        try {
            await navigator.clipboard.writeText(code);
            setCodeCopied(true);
            window.setTimeout(() => setCodeCopied(false), 1500);
        } catch {
            // Silent — clipboard may be unavailable (no permission, no HTTPS/localhost).
        }
    };

    const handleUnlinkChat = async (chatId: number) => {
        setUnlinkingChatId(chatId);
        setUnlinkError(null);
        try {
            const result = await unlinkTelegramChat(agentId, chatId);
            setStatus((prev) => (prev ? { ...prev, allowed_chat_ids: result.allowed_chat_ids } : prev));
        } catch (err) {
            setUnlinkError(err instanceof Error ? err.message : "Failed to unlink chat");
        } finally {
            setUnlinkingChatId(null);
        }
    };

    const handleToggleEnabled = () => {
        if (!status?.has_token) return;
        onConfigChange?.((prev) => ({
            enabled: !(prev?.enabled ?? status.enabled),
            bot_username: prev?.bot_username ?? status.bot_username,
            thread_mode: "dedicated",
            bridge_thread_id: prev?.bridge_thread_id ?? null,
            allowed_chat_ids: prev?.allowed_chat_ids ?? [],
        }));
    };

    if (isCreating) {
        return <p className="text-[13px] text-[var(--modal-text-tertiary)] italic">Save the agent first to set up Telegram.</p>;
    }

    if (statusLoading && !status) {
        return (
            <div className="flex items-center gap-[8px] text-[13px] text-[var(--modal-text-tertiary)]">
                <Loader2 className="w-[14px] h-[14px] animate-spin" /> Loading…
            </div>
        );
    }

    if (statusError && !status) {
        return (
            <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                {statusError}
            </div>
        );
    }

    const pairingExpiresLabel = status?.pending_pairing_code ? formatPairingExpiry(status.pending_pairing_code.expires_at_unix) : null;
    const activePairingCode = pairingExpiresLabel ? status?.pending_pairing_code ?? null : null;
    const linkedChatIds = status?.allowed_chat_ids ?? [];

    return (
        <div className="flex flex-col gap-[16px]">
            <p className="text-[13px] text-[var(--modal-text-secondary)]">
                Connect this agent to a Telegram bot so it can chat over Telegram. Message
                @BotFather on Telegram, run /newbot, and paste the token it gives you below.
            </p>

            <div className="flex items-center justify-between gap-[12px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                <div className="flex flex-col gap-[2px]">
                    <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">Enable Telegram bridge</span>
                    <span className="text-[12px] text-[var(--modal-text-tertiary)]">
                        {status?.has_token ? "Applies the next time you save this agent." : "Set a bot token below first."}
                    </span>
                </div>
                <button
                    type="button"
                    onClick={handleToggleEnabled}
                    disabled={!status?.has_token}
                    role="switch"
                    aria-checked={config?.enabled ?? status?.enabled ?? false}
                    aria-label={(config?.enabled ?? status?.enabled) ? "Disable Telegram bridge" : "Enable Telegram bridge"}
                    className={`relative w-[42px] h-[24px] rounded-full transition-colors flex-shrink-0 ${
                        (config?.enabled ?? status?.enabled) ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"
                    } ${!status?.has_token ? "opacity-50 cursor-default" : "cursor-pointer"}`}
                >
                    <div
                        className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${
                            (config?.enabled ?? status?.enabled) ? "translate-x-[20px]" : "translate-x-[2px]"
                        }`}
                    />
                </button>
            </div>

            {status?.has_token && !showForm && (
                <div className="flex items-center justify-between gap-[12px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                    <div className="flex flex-col gap-[4px]">
                        <span className="text-[14px] font-medium text-[var(--modal-text-primary)] font-mono">@{status.bot_username}</span>
                        <div className="flex items-center gap-[10px] text-[12px]">
                            <span className={`inline-flex items-center gap-[5px] font-medium ${status.enabled ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                                <Check className="w-[12px] h-[12px]" /> {status.enabled ? "Enabled" : "Disabled"}
                            </span>
                            <ChannelConnectionBadge state={connectionState} />
                        </div>
                    </div>
                    <div className="flex items-center gap-[8px]">
                        <button
                            type="button"
                            onClick={() => { setShowForm(true); setSaveError(null); }}
                            className="h-[30px] px-[12px] rounded-[8px] text-[12px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                        >
                            Replace token
                        </button>
                        <button
                            type="button"
                            onClick={handleRemove}
                            disabled={removing}
                            className="h-[30px] px-[12px] rounded-[8px] text-[12px] font-medium text-[#E01E5A] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[6px]"
                        >
                            {removing && <Loader2 className="w-[12px] h-[12px] animate-spin" />}
                            {removing ? "Removing…" : "Disconnect"}
                        </button>
                    </div>
                </div>
            )}

            {removeError && (
                <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                    {removeError}
                </div>
            )}

            {showForm && (
                <div className="flex flex-col gap-[10px]">
                    <div>
                        <Label htmlFor="tg-token">Bot token</Label>
                        <TextInput id="tg-token" value={tokenInput} onChange={setTokenInput} placeholder="123456:ABC-DEF..." monospace />
                    </div>
                    {saveError && (
                        <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                            {saveError}
                        </div>
                    )}
                    <div className="flex items-center gap-[8px]">
                        <button
                            type="button"
                            onClick={handleSave}
                            disabled={saving || !tokenInput.trim()}
                            className="h-[36px] px-[16px] rounded-[8px] text-[13px] font-semibold text-white bg-[#006E51] hover:bg-[#005a43] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[8px]"
                        >
                            {saving && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
                            {saving
                                ? (status?.has_token ? "Updating…" : "Setting…")
                                : (status?.has_token ? "Update Token" : "Set Token")}
                        </button>
                        {status?.has_token && (
                            <button
                                type="button"
                                onClick={() => { setShowForm(false); setTokenInput(""); setSaveError(null); }}
                                disabled={saving}
                                className="h-[36px] px-[14px] rounded-[8px] text-[13px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                            >
                                Cancel
                            </button>
                        )}
                    </div>
                </div>
            )}

            {status?.has_token && (
                <div className="flex flex-col gap-[10px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                    <div className="flex items-center justify-between gap-[12px]">
                        <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">Link a Telegram chat</span>
                        <button
                            type="button"
                            onClick={handleGeneratePairingCode}
                            disabled={pairingLoading}
                            className="h-[30px] px-[12px] rounded-[8px] text-[12px] font-medium text-white bg-[#006E51] hover:bg-[#005a43] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[6px]"
                        >
                            {pairingLoading && <Loader2 className="w-[12px] h-[12px] animate-spin" />}
                            {pairingLoading ? "Generating…" : activePairingCode ? "Generate new code" : "Generate pairing code"}
                        </button>
                    </div>
                    <div>
                        <button
                            type="button"
                            onClick={() => setHowToConnectOpen((open) => !open)}
                            aria-expanded={howToConnectOpen}
                            className="flex items-center gap-[4px] text-[12px] font-medium text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
                        >
                            <ChevronRight className={`w-[12px] h-[12px] transition-transform ${howToConnectOpen ? "rotate-90" : ""}`} />
                            How to connect
                        </button>
                        {howToConnectOpen && (
                            <ul className="mt-[6px] pl-[16px] flex flex-col gap-[4px] text-[12px] text-[var(--modal-text-tertiary)] list-disc">
                                <li>
                                    To connect a DM or a group: generate a pairing code (below), then send{" "}
                                    <span className="font-mono text-[var(--modal-text-secondary)]">/start &lt;code&gt;</span> in that chat.{" "}
                                    <span className="font-mono text-[var(--modal-text-secondary)]">/start@yourbot &lt;code&gt;</span> also works.
                                </li>
                                <li>The code is single-use and expires in 10 minutes.</li>
                                <li>
                                    Pair each DM and each group separately — generate a fresh code for every chat you want to
                                    authorize. Adding a new chat does not remove chats you already paired.
                                </li>
                                <li>
                                    For groups: turn off Group Privacy in BotFather (BotFather → your bot → Group Privacy → Turn
                                    off) so the bot can see @mentions.
                                </li>
                            </ul>
                        )}
                    </div>
                    {pairingError && (
                        <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                            {pairingError}
                        </div>
                    )}
                    {activePairingCode && (
                        <div className="flex flex-col gap-[6px]">
                            <div className="flex items-center gap-[8px]">
                                <span className="font-mono text-[18px] font-semibold tracking-[0.08em] text-[var(--modal-text-primary)] select-all">
                                    {activePairingCode.code}
                                </span>
                                <button
                                    type="button"
                                    onClick={() => handleCopyCode(activePairingCode.code)}
                                    aria-label="Copy pairing code"
                                    title={codeCopied ? "Copied" : "Copy code"}
                                    className="p-[6px] rounded-[6px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                                >
                                    {codeCopied ? <Check className="w-[13px] h-[13px]" /> : <Copy className="w-[13px] h-[13px]" />}
                                </button>
                                <span className="text-[12px] text-[var(--modal-text-tertiary)]">{pairingExpiresLabel}</span>
                            </div>
                            <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                                In Telegram, send /start {activePairingCode.code} to your bot to link this chat.
                            </p>
                        </div>
                    )}
                </div>
            )}

            {status?.has_token && (
                <div className="flex flex-col gap-[8px]">
                    <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">Linked chats</span>
                    {unlinkError && (
                        <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                            {unlinkError}
                        </div>
                    )}
                    {linkedChatIds.length === 0 ? (
                        <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                            No chats linked yet — until you link one, the bot ignores all incoming messages.
                        </p>
                    ) : (
                        <div className="flex flex-col gap-[6px]">
                            {linkedChatIds.map((chatId) => (
                                <div
                                    key={chatId}
                                    className="flex items-center justify-between gap-[12px] px-[12px] py-[8px] rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]"
                                >
                                    <span className="font-mono text-[13px] text-[var(--modal-text-primary)]">{chatId}</span>
                                    <button
                                        type="button"
                                        onClick={() => handleUnlinkChat(chatId)}
                                        disabled={unlinkingChatId === chatId}
                                        className="h-[26px] px-[10px] rounded-[8px] text-[12px] font-medium text-[#E01E5A] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[6px]"
                                    >
                                        {unlinkingChatId === chatId && <Loader2 className="w-[11px] h-[11px] animate-spin" />}
                                        {unlinkingChatId === chatId ? "Unlinking…" : "Unlink"}
                                    </button>
                                </div>
                            ))}
                        </div>
                    )}
                </div>
            )}

            {status?.has_token && (
                <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                    Once enabled, every Telegram chat that messages this agent — each private DM sender or
                    group — gets its own dedicated thread, created automatically the first time that chat sends
                    a message and kept isolated from every other chat's conversation.
                </p>
            )}
        </div>
    );
}

// ─── Email tab ─────────────────────────────────────────────────────────────────

const DEFAULT_IMAP_PORT = "993";
const DEFAULT_SMTP_PORT = "587";
const DEFAULT_POLL_SECS = "300";

/** Deterministic binding id for an agent's (at most one, today) Email
 *  binding, matching the backend's own `EMAIL_BINDING_ID` constant — used to
 *  address the dedicated sender-list endpoints. */
const EMAIL_BINDING_ID = "email";

/** Setup surface for the per-agent Email channel. The whole config here
 *  (including the enable flag) is saved through its own dedicated channel
 *  endpoints rather than riding the AgentProfile PUT — there's no
 *  profile-level draft for any of these fields. It has no Save button of its
 *  own, though: [`ChannelSaveHandle`] exposes `save`/`isConfigured` so the
 *  modal's single primary Save button (bottom of the modal) can persist this
 *  tab too, alongside the profile, in one click. The IMAP/SMTP password is
 *  write-only, the same contract as Telegram's bot token: it lives only in
 *  the input while typed, is sent once, and is never stored or re-rendered
 *  afterward. */
export const EmailTabPanel = forwardRef<ChannelSaveHandle, { agentId: string; isCreating: boolean; onConfiguredChange?: (configured: boolean) => void }>(
function EmailTabPanel({ agentId, isCreating, onConfiguredChange }, ref) {
    const [status, setStatus] = useState<ChannelStatus | null>(null);
    const [statusLoading, setStatusLoading] = useState(false);
    const [statusError, setStatusError] = useState<string | null>(null);

    const [address, setAddress] = useState("");
    const [imapHost, setImapHost] = useState("");
    const [imapPort, setImapPort] = useState(DEFAULT_IMAP_PORT);
    const [smtpHost, setSmtpHost] = useState("");
    const [smtpPort, setSmtpPort] = useState(DEFAULT_SMTP_PORT);
    const [pollSecs, setPollSecs] = useState(DEFAULT_POLL_SECS);
    const [requireAuthResults, setRequireAuthResults] = useState(true);
    const [allowedSenders, setAllowedSenders] = useState<string[]>([]);
    const [enabled, setEnabled] = useState(false);

    const [passwordInput, setPasswordInput] = useState("");
    const [saving, setSaving] = useState(false);
    const [saveError, setSaveError] = useState<string | null>(null);

    const [removing, setRemoving] = useState(false);
    const [removeError, setRemoveError] = useState<string | null>(null);

    // Folds a fresh status into every local form field, including the
    // server-owned ones (bridge thread, secret_stored) — this tab never
    // drafts against a parent-held config the way Telegram's enable flag
    // does, so the status response is the single source of truth. Does NOT
    // touch allowedSenders: `ChannelStatus.allowed_senders` mirrors the
    // deprecated inline profile copy and goes stale the moment the allow-list
    // is edited through the dedicated senders endpoint, so that field is
    // sourced separately (see the load effect and handleSave below).
    const applyStatus = useCallback((s: ChannelStatus | null) => {
        setStatus(s);
        const cfg = (s?.kind_config ?? {}) as Partial<EmailChannelConfig>;
        setAddress(cfg.address ?? "");
        setImapHost(cfg.imap_host ?? "");
        setImapPort(cfg.imap_port ? String(cfg.imap_port) : DEFAULT_IMAP_PORT);
        setSmtpHost(cfg.smtp_host ?? "");
        setSmtpPort(cfg.smtp_port ? String(cfg.smtp_port) : DEFAULT_SMTP_PORT);
        setPollSecs(cfg.poll_secs ? String(cfg.poll_secs) : DEFAULT_POLL_SECS);
        setRequireAuthResults(cfg.require_auth_results ?? true);
        setEnabled(s?.enabled ?? false);
    }, []);

    useEffect(() => {
        if (isCreating) return;
        let cancelled = false;
        setStatusLoading(true);
        setStatusError(null);
        Promise.all([getAgentChannels(agentId), getChannelSenders(agentId, EMAIL_BINDING_ID)])
            .then(([channels, senders]) => {
                if (cancelled) return;
                applyStatus(channels.find((c) => c.kind === "email") ?? null);
                setAllowedSenders(senders.senders);
            })
            .catch((err) => {
                if (cancelled) return;
                setStatusError(err instanceof Error ? err.message : "Failed to load Email channel status");
            })
            .finally(() => {
                if (!cancelled) setStatusLoading(false);
            });
        return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [agentId, isCreating]);

    // Single Save action for the whole tab: the config PUT always carries the
    // complete draft (never a partial patch), and the password PUT — if a new
    // password was typed — only fires after that config PUT succeeds. The
    // allow-list is persisted separately through the dedicated clobber-free
    // senders endpoint (not by relying on the config PUT's echoed
    // allowed_senders, which mirrors the deprecated inline profile copy and
    // can't be trusted as the post-save source of truth). Exactly one
    // applyStatus call reconciles the rest of the form afterward, on the last
    // successful response; that's what keeps a password-only save from
    // clobbering unsaved edits to the other fields.
    const handleSave = useCallback(async (): Promise<{ ok: true } | { ok: false; error: string }> => {
        setSaving(true);
        setSaveError(null);
        try {
            const configResult = await upsertEmailChannel(agentId, {
                address: address.trim(),
                imap_host: imapHost.trim(),
                imap_port: parseInt(imapPort, 10) || 0,
                smtp_host: smtpHost.trim(),
                smtp_port: parseInt(smtpPort, 10) || 0,
                poll_secs: parseInt(pollSecs, 10) || 0,
                require_auth_results: requireAuthResults,
                allowed_senders: allowedSenders,
                enabled,
            });
            const sendersResult = await setChannelSenders(agentId, EMAIL_BINDING_ID, allowedSenders);
            setAllowedSenders(sendersResult.senders);

            const trimmedPassword = passwordInput.trim();
            if (!trimmedPassword) {
                applyStatus(configResult);
                return { ok: true };
            }

            try {
                const secretResult = await setEmailChannelSecret(agentId, trimmedPassword);
                applyStatus(secretResult);
                setPasswordInput("");
                return { ok: true };
            } catch (err) {
                applyStatus(configResult);
                const message = `Configuration was saved, but the password update failed: ${err instanceof Error ? err.message : "unknown error"}`;
                setSaveError(message);
                return { ok: false, error: message };
            }
        } catch (err) {
            const message = err instanceof Error ? err.message : "Failed to save Email configuration";
            setSaveError(message);
            return { ok: false, error: message };
        } finally {
            setSaving(false);
        }
    }, [agentId, address, imapHost, imapPort, smtpHost, smtpPort, pollSecs, requireAuthResults, allowedSenders, enabled, passwordInput, applyStatus]);

    // "Configured" gates both isConfigured() (whether the primary Save button
    // should bother calling save() at all) and, via onConfiguredChange below,
    // whether the primary Save button is even clickable when nothing on the
    // Info/Instructions/etc. tabs changed this session.
    const configured = status !== null || enabled || address.trim() !== "" || imapHost.trim() !== "" || smtpHost.trim() !== "";

    useEffect(() => {
        onConfiguredChange?.(configured);
    }, [configured, onConfiguredChange]);

    useImperativeHandle(ref, () => ({
        isConfigured: () => configured,
        save: handleSave,
    }), [configured, handleSave]);

    const handleRemove = async () => {
        setRemoving(true);
        setRemoveError(null);
        try {
            await deleteEmailChannel(agentId);
            applyStatus(null);
            setAllowedSenders([]);
        } catch (err) {
            setRemoveError(err instanceof Error ? err.message : "Failed to remove Email channel");
        } finally {
            setRemoving(false);
        }
    };

    if (isCreating) {
        return <p className="text-[13px] text-[var(--modal-text-tertiary)] italic">Save the agent first to set up Email.</p>;
    }

    if (statusLoading && !status) {
        return (
            <div className="flex items-center gap-[8px] text-[13px] text-[var(--modal-text-tertiary)]">
                <Loader2 className="w-[14px] h-[14px] animate-spin" /> Loading…
            </div>
        );
    }

    if (statusError && !status) {
        return (
            <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                {statusError}
            </div>
        );
    }

    return (
        <div className="flex flex-col gap-[16px]">
            <p className="text-[13px] text-[var(--modal-text-secondary)]">
                Connect this agent to an email inbox over IMAP/SMTP. Once enabled with credentials, forward or CC
                mail to this address and the agent ingests it as a message — it can reply using its SendEmail tool.
            </p>

            <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                Once enabled, every email conversation — identified by sender and subject — gets its own dedicated
                thread, created automatically the first time it arrives and kept isolated from every other
                sender's conversation.
            </p>

            <div className="flex flex-wrap items-center gap-[16px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] text-[12px]">
                <span className={`inline-flex items-center gap-[5px] font-medium ${status?.enabled ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                    <Check className="w-[12px] h-[12px]" /> {status?.enabled ? "Enabled" : "Disabled"}
                </span>
                <span className={`inline-flex items-center gap-[5px] font-medium ${status?.bridge_thread_provisioned ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                    <Check className="w-[12px] h-[12px]" /> Bridge thread {status?.bridge_thread_provisioned ? "provisioned" : "not yet provisioned"}
                </span>
                <span className={`inline-flex items-center gap-[5px] font-medium ${status?.secret_stored ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                    <Check className="w-[12px] h-[12px]" /> Password {status?.secret_stored ? "set" : "not set"}
                </span>
                <ChannelConnectionBadge state={status?.connection_state} />
            </div>

            <div className="flex flex-col gap-[10px]">
                <div>
                    <Label htmlFor="email-address">Email address</Label>
                    <TextInput id="email-address" value={address} onChange={setAddress} placeholder="agent@yourdomain.com" monospace />
                </div>
                <div className="grid grid-cols-2 gap-[10px]">
                    <div>
                        <Label htmlFor="email-imap-host">IMAP host</Label>
                        <TextInput id="email-imap-host" value={imapHost} onChange={setImapHost} placeholder="imap.yourdomain.com" monospace />
                    </div>
                    <div>
                        <Label htmlFor="email-imap-port">IMAP port</Label>
                        <TextInput id="email-imap-port" value={imapPort} onChange={setImapPort} placeholder={DEFAULT_IMAP_PORT} monospace />
                    </div>
                </div>
                <div className="grid grid-cols-2 gap-[10px]">
                    <div>
                        <Label htmlFor="email-smtp-host">SMTP host</Label>
                        <TextInput id="email-smtp-host" value={smtpHost} onChange={setSmtpHost} placeholder="smtp.yourdomain.com" monospace />
                    </div>
                    <div>
                        <Label htmlFor="email-smtp-port">SMTP port</Label>
                        <TextInput id="email-smtp-port" value={smtpPort} onChange={setSmtpPort} placeholder={DEFAULT_SMTP_PORT} monospace />
                    </div>
                </div>
                <div>
                    <Label htmlFor="email-poll-secs">Poll interval (seconds)</Label>
                    <TextInput id="email-poll-secs" value={pollSecs} onChange={setPollSecs} placeholder={DEFAULT_POLL_SECS} monospace />
                </div>

                <div className="flex items-center justify-between gap-[12px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                    <div className="flex flex-col gap-[2px]">
                        <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">Require authentication results</span>
                        <span className="text-[12px] text-[var(--modal-text-tertiary)]">
                            Rejects inbound mail that fails SPF/DKIM/DMARC, so a spoofed sender can't impersonate someone
                            on the allow-list below. Leave this on unless you have a specific reason to turn it off.
                        </span>
                    </div>
                    <button
                        type="button"
                        onClick={() => setRequireAuthResults((v) => !v)}
                        role="switch"
                        aria-checked={requireAuthResults}
                        aria-label={requireAuthResults ? "Disable authentication result requirement" : "Require authentication results"}
                        className={`relative w-[42px] h-[24px] rounded-full transition-colors flex-shrink-0 cursor-pointer ${requireAuthResults ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"}`}
                    >
                        <div className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${requireAuthResults ? "translate-x-[20px]" : "translate-x-[2px]"}`} />
                    </button>
                </div>

                <div>
                    <Label htmlFor="email-allowed-senders">Allowed senders</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        Full addresses (user@example.com) or @domain entries. Leave empty to reject all inbound mail.
                    </p>
                    <StringListEditor id="email-allowed-senders" values={allowedSenders} onChange={setAllowedSenders} placeholder="user@example.com or @example.com" />
                </div>

                <div className="flex items-center justify-between gap-[12px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                    <div className="flex flex-col gap-[2px]">
                        <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">Enable Email channel</span>
                        <span className="text-[12px] text-[var(--modal-text-tertiary)]">Applies the next time you save this agent.</span>
                    </div>
                    <button
                        type="button"
                        onClick={() => setEnabled((v) => !v)}
                        role="switch"
                        aria-checked={enabled}
                        aria-label={enabled ? "Disable Email channel" : "Enable Email channel"}
                        className={`relative w-[42px] h-[24px] rounded-full transition-colors flex-shrink-0 cursor-pointer ${enabled ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"}`}
                    >
                        <div className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${enabled ? "translate-x-[20px]" : "translate-x-[2px]"}`} />
                    </button>
                </div>

                <div>
                    <Label htmlFor="email-password">IMAP/SMTP password</Label>
                    <TextInput
                        id="email-password"
                        value={passwordInput}
                        onChange={setPasswordInput}
                        placeholder={status?.secret_stored ? "Leave blank to keep current password" : "App password"}
                        monospace
                    />
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mt-[4px]">
                        {status?.secret_stored
                            ? "Password saved — leave blank to keep it, or enter a new one to replace it."
                            : "Required to enable IMAP/SMTP access."}
                    </p>
                </div>

                {saveError && (
                    <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                        {saveError}
                    </div>
                )}
                {saving && (
                    <p className="flex items-center gap-[6px] text-[12px] text-[var(--modal-text-tertiary)]">
                        <Loader2 className="w-[12px] h-[12px] animate-spin" /> Saving Email configuration…
                    </p>
                )}
            </div>

            {removeError && (
                <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                    {removeError}
                </div>
            )}
            {status && (
                <div>
                    <button
                        type="button"
                        onClick={handleRemove}
                        disabled={removing}
                        className="h-[30px] px-[12px] rounded-[8px] text-[12px] font-medium text-[#E01E5A] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[6px]"
                    >
                        {removing && <Loader2 className="w-[12px] h-[12px] animate-spin" />}
                        {removing ? "Removing…" : "Remove Email channel"}
                    </button>
                </div>
            )}
        </div>
    );
});

// ─── Discord tab ────────────────────────────────────────────────────────────────

/** Setup surface for the per-agent Discord channel. Same self-contained
 *  contract as [`EmailTabPanel`]: the whole config (including the enable
 *  flag) is saved directly through the channel endpoints rather than riding
 *  the AgentProfile PUT, and it has no Save button of its own — it exposes
 *  [`ChannelSaveHandle`] so the modal's single primary Save button can
 *  persist it. The bot token is write-only, the same contract as Email's
 *  password: it lives only in the input while typed, is sent once, and is
 *  never stored or re-rendered afterward. It stays a separate, distinct
 *  action (its own "Set/Replace bot token" button) rather than folding into
 *  the bulk save, matching the precedent every other channel already
 *  follows for its own write-only secret. */
const DEFAULT_THREAD_IDLE_TIMEOUT_MINUTES = "15";
const DEFAULT_THREAD_MESSAGE_BUDGET = "10";
const DEFAULT_BACKFILL_LIMIT = "20";

const THREAD_FOLLOW_OPTIONS: { label: string; value: ThreadFollowMode }[] = [
    { label: "Answer once, then wait for another mention", value: "one_shot" },
    { label: "Stay in the conversation briefly", value: "sticky_decay" },
    { label: "Stay in the conversation permanently", value: "always" },
];

export const DiscordTabPanel = forwardRef<ChannelSaveHandle, { agentId: string; isCreating: boolean; onConfiguredChange?: (configured: boolean) => void }>(
function DiscordTabPanel({ agentId, isCreating, onConfiguredChange }, ref) {
    const [status, setStatus] = useState<ChannelStatus | null>(null);
    const [statusLoading, setStatusLoading] = useState(false);
    const [statusError, setStatusError] = useState<string | null>(null);

    const [allowedUsers, setAllowedUsers] = useState<string[]>([]);
    const [allowedRoles, setAllowedRoles] = useState<string[]>([]);
    const [allowedChannels, setAllowedChannels] = useState<string[]>([]);
    const [dmRoleAuthGuild, setDmRoleAuthGuild] = useState("");
    const [requireMention, setRequireMention] = useState(true);
    const [threadFollow, setThreadFollow] = useState<ThreadFollowMode>("sticky_decay");
    const [threadIdleTimeoutMinutes, setThreadIdleTimeoutMinutes] = useState(DEFAULT_THREAD_IDLE_TIMEOUT_MINUTES);
    const [threadMessageBudget, setThreadMessageBudget] = useState(DEFAULT_THREAD_MESSAGE_BUDGET);
    const [backfillLimit, setBackfillLimit] = useState(DEFAULT_BACKFILL_LIMIT);
    const [enabled, setEnabled] = useState(false);

    const [savingConfig, setSavingConfig] = useState(false);
    const [configError, setConfigError] = useState<string | null>(null);

    const [tokenInput, setTokenInput] = useState("");
    const [savingToken, setSavingToken] = useState(false);
    const [tokenError, setTokenError] = useState<string | null>(null);

    const [removing, setRemoving] = useState(false);
    const [removeError, setRemoveError] = useState<string | null>(null);

    // Folds a fresh status into every local form field, including the
    // server-owned ones (bridge thread, secret_stored) — this tab never
    // drafts against a parent-held config the way Telegram's enable flag
    // does, so the status response is the single source of truth.
    const applyStatus = useCallback((s: ChannelStatus | null) => {
        setStatus(s);
        const cfg = (s?.kind_config ?? {}) as Partial<DiscordChannelConfig>;
        setAllowedUsers(cfg.allowed_users ?? []);
        setAllowedRoles(cfg.allowed_roles ?? []);
        setAllowedChannels(cfg.allowed_channels ?? []);
        setDmRoleAuthGuild(cfg.dm_role_auth_guild ?? "");
        setRequireMention(cfg.require_mention ?? true);
        setThreadFollow(cfg.thread_follow ?? "sticky_decay");
        setThreadIdleTimeoutMinutes(
            cfg.thread_idle_timeout_minutes ? String(cfg.thread_idle_timeout_minutes) : DEFAULT_THREAD_IDLE_TIMEOUT_MINUTES,
        );
        setThreadMessageBudget(
            cfg.thread_message_budget ? String(cfg.thread_message_budget) : DEFAULT_THREAD_MESSAGE_BUDGET,
        );
        setBackfillLimit(
            cfg.backfill_limit !== undefined && cfg.backfill_limit !== null ? String(cfg.backfill_limit) : DEFAULT_BACKFILL_LIMIT,
        );
        setEnabled(s?.enabled ?? false);
    }, []);

    useEffect(() => {
        if (isCreating) return;
        let cancelled = false;
        setStatusLoading(true);
        setStatusError(null);
        getAgentChannels(agentId)
            .then((channels) => {
                if (cancelled) return;
                applyStatus(channels.find((c) => c.kind === "discord") ?? null);
            })
            .catch((err) => {
                if (cancelled) return;
                setStatusError(err instanceof Error ? err.message : "Failed to load Discord channel status");
            })
            .finally(() => {
                if (!cancelled) setStatusLoading(false);
            });
        return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [agentId, isCreating]);

    const handleSaveConfig = useCallback(async (): Promise<{ ok: true } | { ok: false; error: string }> => {
        setSavingConfig(true);
        setConfigError(null);
        try {
            const result = await upsertDiscordChannel(agentId, {
                allowed_users: allowedUsers,
                allowed_roles: allowedRoles,
                allowed_channels: allowedChannels,
                dm_role_auth_guild: dmRoleAuthGuild.trim() || null,
                require_mention: requireMention,
                thread_follow: threadFollow,
                thread_idle_timeout_minutes: parseInt(threadIdleTimeoutMinutes, 10) || 0,
                thread_message_budget: parseInt(threadMessageBudget, 10) || 0,
                backfill_limit: parseInt(backfillLimit, 10) || 0,
                enabled,
            });
            applyStatus(result);
            return { ok: true };
        } catch (err) {
            const message = err instanceof Error ? err.message : "Failed to save Discord configuration";
            setConfigError(message);
            return { ok: false, error: message };
        } finally {
            setSavingConfig(false);
        }
    }, [agentId, allowedUsers, allowedRoles, allowedChannels, dmRoleAuthGuild, requireMention, threadFollow, threadIdleTimeoutMinutes, threadMessageBudget, backfillLimit, enabled, applyStatus]);

    const configured = status !== null || enabled || allowedUsers.length > 0 || allowedRoles.length > 0 || allowedChannels.length > 0 || dmRoleAuthGuild.trim() !== "";

    useEffect(() => {
        onConfiguredChange?.(configured);
    }, [configured, onConfiguredChange]);

    useImperativeHandle(ref, () => ({
        isConfigured: () => configured,
        save: handleSaveConfig,
    }), [configured, handleSaveConfig]);

    const handleSetToken = async () => {
        const trimmed = tokenInput.trim();
        if (!trimmed) return;
        setSavingToken(true);
        setTokenError(null);
        try {
            const result = await setDiscordChannelSecret(agentId, trimmed);
            applyStatus(result);
            setTokenInput("");
        } catch (err) {
            setTokenError(err instanceof Error ? err.message : "Failed to save bot token");
        } finally {
            setSavingToken(false);
        }
    };

    const handleRemove = async () => {
        setRemoving(true);
        setRemoveError(null);
        try {
            await deleteDiscordChannel(agentId);
            applyStatus(null);
        } catch (err) {
            setRemoveError(err instanceof Error ? err.message : "Failed to remove Discord channel");
        } finally {
            setRemoving(false);
        }
    };

    if (isCreating) {
        return <p className="text-[13px] text-[var(--modal-text-tertiary)] italic">Save the agent first to set up Discord.</p>;
    }

    if (statusLoading && !status) {
        return (
            <div className="flex items-center gap-[8px] text-[13px] text-[var(--modal-text-tertiary)]">
                <Loader2 className="w-[14px] h-[14px] animate-spin" /> Loading…
            </div>
        );
    }

    if (statusError && !status) {
        return (
            <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                {statusError}
            </div>
        );
    }

    return (
        <div className="flex flex-col gap-[16px]">
            <p className="text-[13px] text-[var(--modal-text-secondary)]">
                Connect this agent to a Discord bot. Once enabled with a bot token, mentions and DMs from an allowed
                user or role are ingested as messages — the agent's reply is relayed back to the channel or DM it
                arrived on.
            </p>

            <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                Once enabled, every Discord conversation that reaches this agent — each DM sender or server
                channel — gets its own dedicated thread, created automatically the first time a message arrives
                there and kept isolated from every other conversation.
            </p>

            <div className="flex flex-wrap items-center gap-[16px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] text-[12px]">
                <span className={`inline-flex items-center gap-[5px] font-medium ${status?.enabled ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                    <Check className="w-[12px] h-[12px]" /> {status?.enabled ? "Enabled" : "Disabled"}
                </span>
                <span className={`inline-flex items-center gap-[5px] font-medium ${status?.bridge_thread_provisioned ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                    <Check className="w-[12px] h-[12px]" /> Bridge thread {status?.bridge_thread_provisioned ? "provisioned" : "not yet provisioned"}
                </span>
                <span className={`inline-flex items-center gap-[5px] font-medium ${status?.secret_stored ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                    <Check className="w-[12px] h-[12px]" /> Bot token {status?.secret_stored ? "set" : "not set"}
                </span>
                <ChannelConnectionBadge state={status?.connection_state} />
            </div>

            <div className="flex flex-col gap-[10px]">
                <div>
                    <Label htmlFor="discord-allowed-users">Allowed users</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        Discord user IDs (or usernames) permitted to trigger the agent. OR-combined with allowed roles below.
                    </p>
                    <StringListEditor id="discord-allowed-users" values={allowedUsers} onChange={setAllowedUsers} placeholder="user id or username" />
                </div>

                <div>
                    <Label htmlFor="discord-allowed-roles">Allowed roles</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        Guild role IDs permitted to trigger the agent. OR-combined with allowed users above.
                    </p>
                    <StringListEditor id="discord-allowed-roles" values={allowedRoles} onChange={setAllowedRoles} placeholder="role id" />
                </div>

                <div>
                    <Label htmlFor="discord-allowed-channels">Allowed channels</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        Optional channel-ID allow-list. Leave empty to allow every channel the bot can see, subject to
                        the user/role checks above.
                    </p>
                    <StringListEditor id="discord-allowed-channels" values={allowedChannels} onChange={setAllowedChannels} placeholder="channel id" />
                </div>

                <div>
                    <Label htmlFor="discord-dm-role-auth-guild">DM role-auth guild</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        Guild whose roles authorize direct messages, since a DM has no guild of its own to resolve
                        roles against. Leave blank to disable role-based auth for DMs (only allowed users apply there).
                    </p>
                    <TextInput id="discord-dm-role-auth-guild" value={dmRoleAuthGuild} onChange={setDmRoleAuthGuild} placeholder="guild id" monospace />
                </div>

                <div className="flex items-center justify-between gap-[12px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                    <div className="flex flex-col gap-[2px]">
                        <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">Only respond when mentioned</span>
                        <span className="text-[12px] text-[var(--modal-text-tertiary)]">
                            When off, the agent replies to every message from an allowed user or role in a channel it
                            can see, not just ones that @-mention it. Direct messages always get a reply either way.
                        </span>
                    </div>
                    <button
                        type="button"
                        onClick={() => setRequireMention((v) => !v)}
                        role="switch"
                        aria-checked={requireMention}
                        aria-label={requireMention ? "Respond to every message, not just mentions" : "Only respond when mentioned"}
                        className={`relative w-[42px] h-[24px] rounded-full transition-colors flex-shrink-0 cursor-pointer ${requireMention ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"}`}
                    >
                        <div className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${requireMention ? "translate-x-[20px]" : "translate-x-[2px]"}`} />
                    </button>
                </div>

                <div>
                    <Label htmlFor="discord-thread-follow">Conversation follow-up</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        After the agent replies to a mention inside a thread, how long it keeps replying there without
                        needing another mention.
                    </p>
                    <FormSelect
                        id="discord-thread-follow"
                        value={threadFollow}
                        onChange={(v) => setThreadFollow(v as ThreadFollowMode)}
                        options={THREAD_FOLLOW_OPTIONS}
                    />
                </div>

                {threadFollow === "sticky_decay" && (
                    <div className="grid grid-cols-2 gap-[10px]">
                        <div>
                            <Label htmlFor="discord-thread-idle-timeout">Idle timeout (minutes)</Label>
                            <TextInput
                                id="discord-thread-idle-timeout"
                                value={threadIdleTimeoutMinutes}
                                onChange={setThreadIdleTimeoutMinutes}
                                placeholder={DEFAULT_THREAD_IDLE_TIMEOUT_MINUTES}
                                monospace
                            />
                        </div>
                        <div>
                            <Label htmlFor="discord-thread-message-budget">Messages before going quiet</Label>
                            <TextInput
                                id="discord-thread-message-budget"
                                value={threadMessageBudget}
                                onChange={setThreadMessageBudget}
                                placeholder={DEFAULT_THREAD_MESSAGE_BUDGET}
                                monospace
                            />
                        </div>
                    </div>
                )}

                <div>
                    <Label htmlFor="discord-backfill-limit">History to read on first reply</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        Number of earlier messages the agent reads for context the first time it replies in a
                        conversation. Set to 0 to turn this off. Requires the bot's "Read Message History" permission
                        in the server.
                    </p>
                    <TextInput id="discord-backfill-limit" value={backfillLimit} onChange={setBackfillLimit} placeholder={DEFAULT_BACKFILL_LIMIT} monospace />
                </div>

                <div className="flex items-center justify-between gap-[12px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                    <div className="flex flex-col gap-[2px]">
                        <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">Enable Discord channel</span>
                        <span className="text-[12px] text-[var(--modal-text-tertiary)]">Applies the next time you save this agent.</span>
                    </div>
                    <button
                        type="button"
                        onClick={() => setEnabled((v) => !v)}
                        role="switch"
                        aria-checked={enabled}
                        aria-label={enabled ? "Disable Discord channel" : "Enable Discord channel"}
                        className={`relative w-[42px] h-[24px] rounded-full transition-colors flex-shrink-0 cursor-pointer ${enabled ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"}`}
                    >
                        <div className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${enabled ? "translate-x-[20px]" : "translate-x-[2px]"}`} />
                    </button>
                </div>

                {configError && (
                    <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                        {configError}
                    </div>
                )}
                {savingConfig && (
                    <p className="flex items-center gap-[6px] text-[12px] text-[var(--modal-text-tertiary)]">
                        <Loader2 className="w-[12px] h-[12px] animate-spin" /> Saving Discord configuration…
                    </p>
                )}
            </div>

            <div className="flex flex-col gap-[10px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">
                    Bot token — {status?.secret_stored ? "set" : "not set"}
                </span>
                <div>
                    <Label htmlFor="discord-bot-token">Bot token</Label>
                    <TextInput id="discord-bot-token" value={tokenInput} onChange={setTokenInput} placeholder="Bot token" monospace />
                </div>
                {tokenError && (
                    <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                        {tokenError}
                    </div>
                )}
                <div>
                    <button
                        type="button"
                        onClick={handleSetToken}
                        disabled={savingToken || !tokenInput.trim()}
                        className="h-[36px] px-[16px] rounded-[8px] text-[13px] font-semibold text-white bg-[#006E51] hover:bg-[#005a43] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[8px]"
                    >
                        {savingToken && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
                        {savingToken ? "Saving…" : status?.secret_stored ? "Replace bot token" : "Set bot token"}
                    </button>
                </div>
            </div>

            {removeError && (
                <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                    {removeError}
                </div>
            )}
            {status && (
                <div>
                    <button
                        type="button"
                        onClick={handleRemove}
                        disabled={removing}
                        className="h-[30px] px-[12px] rounded-[8px] text-[12px] font-medium text-[#E01E5A] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[6px]"
                    >
                        {removing && <Loader2 className="w-[12px] h-[12px] animate-spin" />}
                        {removing ? "Removing…" : "Remove Discord channel"}
                    </button>
                </div>
            )}
        </div>
    );
});

// ─── Slack tab ──────────────────────────────────────────────────────────────────

/** Setup surface for the per-agent Slack channel, cloned from
 *  [`DiscordTabPanel`] (persistent-socket precedent, not Telegram's
 *  stateless long-poll shape) — same self-contained contract: the whole
 *  config (including the enable flag) is saved directly through the channel
 *  endpoints rather than riding the AgentProfile PUT, and it has no Save
 *  button of its own — it exposes [`ChannelSaveHandle`] so the modal's
 *  single primary Save button can persist it. Slack needs *two* write-only
 *  tokens (bot `xoxb-`/app `xapp-`) posted once to the single secret
 *  endpoint — both live only in transient state while typed and are never
 *  stored or re-rendered afterward; stored state is read only from
 *  `status.secret_stored`, never from the inputs themselves. Token entry and
 *  Test Connection stay separate, distinct actions (their own buttons)
 *  rather than folding into the bulk save — tokens for the same write-only
 *  secret reason as Discord's/Telegram's, and Test Connection is a
 *  read-only diagnostic against already-saved tokens, not a save at all. */
const SLACK_CONVERSATION_MODE_OPTIONS: { label: string; value: SlackConversationMode }[] = [
    { label: "One thread per conversation", value: "per_conversation" },
];

export const SlackTabPanel = forwardRef<ChannelSaveHandle, { agentId: string; isCreating: boolean; onConfiguredChange?: (configured: boolean) => void }>(
function SlackTabPanel({ agentId, isCreating, onConfiguredChange }, ref) {
    const [status, setStatus] = useState<ChannelStatus | null>(null);
    const [statusLoading, setStatusLoading] = useState(false);
    const [statusError, setStatusError] = useState<string | null>(null);

    const [allowedUsers, setAllowedUsers] = useState<string[]>([]);
    const [allowedChannels, setAllowedChannels] = useState<string[]>([]);
    const [conversationMode, setConversationMode] = useState<SlackConversationMode>("per_conversation");
    const [enabled, setEnabled] = useState(false);

    const [savingConfig, setSavingConfig] = useState(false);
    const [configError, setConfigError] = useState<string | null>(null);

    const [botTokenInput, setBotTokenInput] = useState("");
    const [appTokenInput, setAppTokenInput] = useState("");
    const [savingTokens, setSavingTokens] = useState(false);
    const [tokenError, setTokenError] = useState<string | null>(null);

    const [manifest, setManifest] = useState<string | null>(null);
    const [manifestLoading, setManifestLoading] = useState(false);
    const [manifestError, setManifestError] = useState<string | null>(null);
    const [manifestCopied, setManifestCopied] = useState(false);

    const [testReport, setTestReport] = useState<SlackTestConnectionReport | null>(null);
    const [testRunning, setTestRunning] = useState(false);
    const [testError, setTestError] = useState<string | null>(null);

    const [removing, setRemoving] = useState(false);
    const [removeError, setRemoveError] = useState<string | null>(null);

    // Folds a fresh status into every local form field, including the
    // server-owned ones (bridge thread, secret_stored) — this tab never
    // drafts against a parent-held config, so the status response is the
    // single source of truth.
    const applyStatus = useCallback((s: ChannelStatus | null) => {
        setStatus(s);
        const cfg = (s?.kind_config ?? {}) as Partial<SlackChannelConfig>;
        setAllowedUsers(cfg.allowed_users ?? []);
        setAllowedChannels(cfg.allowed_channels ?? []);
        setConversationMode(cfg.conversation_mode ?? "per_conversation");
        setEnabled(s?.enabled ?? false);
    }, []);

    useEffect(() => {
        if (isCreating) return;
        let cancelled = false;
        setStatusLoading(true);
        setStatusError(null);
        getAgentChannels(agentId)
            .then((channels) => {
                if (cancelled) return;
                applyStatus(channels.find((c) => c.kind === "slack") ?? null);
            })
            .catch((err) => {
                if (cancelled) return;
                setStatusError(err instanceof Error ? err.message : "Failed to load Slack channel status");
            })
            .finally(() => {
                if (!cancelled) setStatusLoading(false);
            });
        return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [agentId, isCreating]);

    useEffect(() => {
        if (isCreating) return;
        let cancelled = false;
        setManifestLoading(true);
        setManifestError(null);
        getSlackManifest(agentId)
            .then((res) => {
                if (cancelled) return;
                setManifest(res.manifest_yaml);
            })
            .catch((err) => {
                if (cancelled) return;
                setManifestError(err instanceof Error ? err.message : "Failed to load app manifest");
            })
            .finally(() => {
                if (!cancelled) setManifestLoading(false);
            });
        return () => { cancelled = true; };
    }, [agentId, isCreating]);

    const handleSaveConfig = useCallback(async (): Promise<{ ok: true } | { ok: false; error: string }> => {
        setSavingConfig(true);
        setConfigError(null);
        try {
            const result = await upsertSlackChannel(agentId, {
                allowed_users: allowedUsers,
                allowed_channels: allowedChannels,
                conversation_mode: conversationMode,
                enabled,
            });
            applyStatus(result);
            return { ok: true };
        } catch (err) {
            const message = err instanceof Error ? err.message : "Failed to save Slack configuration";
            setConfigError(message);
            return { ok: false, error: message };
        } finally {
            setSavingConfig(false);
        }
    }, [agentId, allowedUsers, allowedChannels, conversationMode, enabled, applyStatus]);

    const configured = status !== null || enabled || allowedUsers.length > 0 || allowedChannels.length > 0;

    useEffect(() => {
        onConfiguredChange?.(configured);
    }, [configured, onConfiguredChange]);

    useImperativeHandle(ref, () => ({
        isConfigured: () => configured,
        save: handleSaveConfig,
    }), [configured, handleSaveConfig]);

    const handleSaveTokens = async () => {
        const bot = botTokenInput.trim();
        const app = appTokenInput.trim();
        if (!bot || !app) return;
        setSavingTokens(true);
        setTokenError(null);
        try {
            const result = await setSlackChannelSecret(agentId, bot, app);
            applyStatus(result);
            setBotTokenInput("");
            setAppTokenInput("");
        } catch (err) {
            setTokenError(err instanceof Error ? err.message : "Failed to save Slack tokens");
        } finally {
            setSavingTokens(false);
        }
    };

    const handleCopyManifest = async () => {
        if (!manifest) return;
        try {
            await navigator.clipboard.writeText(manifest);
            setManifestCopied(true);
            window.setTimeout(() => setManifestCopied(false), 1500);
        } catch {
            // Silent — clipboard may be unavailable (no permission, no HTTPS/localhost).
        }
    };

    const handleTestConnection = async () => {
        setTestRunning(true);
        setTestError(null);
        try {
            setTestReport(await testSlackConnection(agentId));
        } catch (err) {
            setTestError(err instanceof Error ? err.message : "Failed to run Test Connection");
        } finally {
            setTestRunning(false);
        }
    };

    const handleRemove = async () => {
        setRemoving(true);
        setRemoveError(null);
        try {
            await deleteSlackChannel(agentId);
            applyStatus(null);
            setTestReport(null);
        } catch (err) {
            setRemoveError(err instanceof Error ? err.message : "Failed to remove Slack channel");
        } finally {
            setRemoving(false);
        }
    };

    if (isCreating) {
        return <p className="text-[13px] text-[var(--modal-text-tertiary)] italic">Save the agent first to set up Slack.</p>;
    }

    if (statusLoading && !status) {
        return (
            <div className="flex items-center gap-[8px] text-[13px] text-[var(--modal-text-tertiary)]">
                <Loader2 className="w-[14px] h-[14px] animate-spin" /> Loading…
            </div>
        );
    }

    if (statusError && !status) {
        return (
            <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                {statusError}
            </div>
        );
    }

    return (
        <div className="flex flex-col gap-[16px]">
            <p className="text-[13px] text-[var(--modal-text-secondary)]">
                Use this panel to connect this agent to a Slack workspace. Once enabled with both tokens, mentions and
                DMs from an allowed channel or user are ingested as messages — the agent's reply is relayed back to
                the conversation it arrived on.
            </p>

            <div className="flex flex-wrap items-center gap-[16px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] text-[12px]">
                <span className={`inline-flex items-center gap-[5px] font-medium ${status?.enabled ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                    <Check className="w-[12px] h-[12px]" /> {status?.enabled ? "Enabled" : "Disabled"}
                </span>
                <span className={`inline-flex items-center gap-[5px] font-medium ${status?.bridge_thread_provisioned ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                    <Check className="w-[12px] h-[12px]" /> Bridge thread {status?.bridge_thread_provisioned ? "provisioned" : "not yet provisioned"}
                </span>
                <span className={`inline-flex items-center gap-[5px] font-medium ${status?.secret_stored ? "text-green-600" : "text-[var(--modal-text-tertiary)]"}`}>
                    <Check className="w-[12px] h-[12px]" /> Tokens {status?.secret_stored ? "set" : "not set"}
                </span>
                <ChannelConnectionBadge state={status?.connection_state} />
            </div>

            <div className="flex flex-col gap-[10px]">
                <div>
                    <Label htmlFor="slack-allowed-users">Allowed users</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        Slack user IDs (`U…`) permitted to trigger the agent. Empty rejects every conversation until
                        at least one user or channel below is added.
                    </p>
                    <StringListEditor id="slack-allowed-users" values={allowedUsers} onChange={setAllowedUsers} placeholder="user id" />
                </div>

                <div>
                    <Label htmlFor="slack-allowed-channels">Allowed channels</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        Slack channel IDs (`C…`/`D…`/`G…`) permitted to trigger the agent. Empty rejects every
                        conversation until at least one channel or user above is added.
                    </p>
                    <StringListEditor id="slack-allowed-channels" values={allowedChannels} onChange={setAllowedChannels} placeholder="channel id" />
                </div>

                <div>
                    <Label htmlFor="slack-conversation-mode">Conversation mode</Label>
                    <p className="text-[12px] text-[var(--modal-text-tertiary)] mb-[6px]">
                        How a Slack conversation maps onto a Launchpad thread. Only one mode exists today.
                    </p>
                    <FormSelect
                        id="slack-conversation-mode"
                        value={conversationMode}
                        onChange={(v) => setConversationMode(v as SlackConversationMode)}
                        options={SLACK_CONVERSATION_MODE_OPTIONS}
                    />
                </div>

                <div className="flex items-center justify-between gap-[12px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                    <div className="flex flex-col gap-[2px]">
                        <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">Enable Slack channel</span>
                        <span className="text-[12px] text-[var(--modal-text-tertiary)]">Applies the next time you save this agent.</span>
                    </div>
                    <button
                        type="button"
                        onClick={() => setEnabled((v) => !v)}
                        role="switch"
                        aria-checked={enabled}
                        aria-label={enabled ? "Disable Slack channel" : "Enable Slack channel"}
                        className={`relative w-[42px] h-[24px] rounded-full transition-colors flex-shrink-0 cursor-pointer ${enabled ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"}`}
                    >
                        <div className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${enabled ? "translate-x-[20px]" : "translate-x-[2px]"}`} />
                    </button>
                </div>

                {configError && (
                    <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                        {configError}
                    </div>
                )}
                {savingConfig && (
                    <p className="flex items-center gap-[6px] text-[12px] text-[var(--modal-text-tertiary)]">
                        <Loader2 className="w-[12px] h-[12px] animate-spin" /> Saving Slack configuration…
                    </p>
                )}
            </div>

            <div className="flex flex-col gap-[10px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                <div className="flex items-center justify-between gap-[8px]">
                    <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">App manifest</span>
                    <button
                        type="button"
                        onClick={handleCopyManifest}
                        disabled={!manifest}
                        title={manifestCopied ? "Copied" : "Copy manifest"}
                        aria-label="Copy app manifest"
                        className="p-[6px] rounded-[6px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                    >
                        {manifestCopied ? <Check className="w-[13px] h-[13px]" /> : <Copy className="w-[13px] h-[13px]" />}
                    </button>
                </div>
                <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                    Paste this into Slack's "Create New App → From an app manifest" flow, then come back and paste the
                    two tokens it gives you below.
                </p>
                {manifestLoading && !manifest && (
                    <div className="flex items-center gap-[8px] text-[13px] text-[var(--modal-text-tertiary)]">
                        <Loader2 className="w-[14px] h-[14px] animate-spin" /> Generating…
                    </div>
                )}
                {manifestError && !manifest && (
                    <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                        {manifestError}
                    </div>
                )}
                {manifest && (
                    <pre className="w-full max-h-[220px] overflow-auto px-[12px] py-[10px] rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[11px] font-mono text-[var(--modal-text-primary)] whitespace-pre-wrap">
                        {manifest}
                    </pre>
                )}
            </div>

            <div className="flex flex-col gap-[10px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">
                    Tokens — {status?.secret_stored ? "set" : "not set"}
                </span>
                <div>
                    <Label htmlFor="slack-bot-token">Bot token</Label>
                    <TextInput id="slack-bot-token" value={botTokenInput} onChange={setBotTokenInput} placeholder="xoxb-…" monospace />
                </div>
                <div>
                    <Label htmlFor="slack-app-token">App-level token</Label>
                    <TextInput id="slack-app-token" value={appTokenInput} onChange={setAppTokenInput} placeholder="xapp-…" monospace />
                </div>
                {tokenError && (
                    <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                        {tokenError}
                    </div>
                )}
                <div>
                    <button
                        type="button"
                        onClick={handleSaveTokens}
                        disabled={savingTokens || !botTokenInput.trim() || !appTokenInput.trim()}
                        className="h-[36px] px-[16px] rounded-[8px] text-[13px] font-semibold text-white bg-[#006E51] hover:bg-[#005a43] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[8px]"
                    >
                        {savingTokens && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
                        {savingTokens ? "Saving…" : status?.secret_stored ? "Replace tokens" : "Save tokens"}
                    </button>
                </div>
            </div>

            <div className="flex flex-col gap-[10px] px-[14px] py-[12px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                <div className="flex items-center justify-between gap-[8px]">
                    <span className="text-[13px] font-medium text-[var(--modal-text-primary)]">Test Connection</span>
                    <button
                        type="button"
                        onClick={handleTestConnection}
                        disabled={testRunning || !status?.secret_stored}
                        title={!status?.secret_stored ? "Save both tokens first" : undefined}
                        className="h-[30px] px-[12px] rounded-[8px] text-[12px] font-semibold text-white bg-[#006E51] hover:bg-[#005a43] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[6px]"
                    >
                        {testRunning && <Loader2 className="w-[12px] h-[12px] animate-spin" />}
                        {testRunning ? "Testing…" : "Test Connection"}
                    </button>
                </div>

                {testError && (
                    <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                        {testError}
                    </div>
                )}

                {testReport && (
                    <div className="flex flex-col gap-[10px] text-[12px]">
                        <div className="flex flex-wrap gap-[16px]">
                            <span className={`inline-flex items-center gap-[5px] font-medium ${testReport.auth_check.passed ? "text-green-600" : "text-[var(--error)]"}`}>
                                {testReport.auth_check.passed ? <Check className="w-[12px] h-[12px]" /> : <X className="w-[12px] h-[12px]" />}
                                Auth check {testReport.auth_check.passed ? "passed" : "failed"}
                            </span>
                            <span className={`inline-flex items-center gap-[5px] font-medium ${testReport.connections_open_check.passed ? "text-green-600" : "text-[var(--error)]"}`}>
                                {testReport.connections_open_check.passed ? <Check className="w-[12px] h-[12px]" /> : <X className="w-[12px] h-[12px]" />}
                                Socket handshake {testReport.connections_open_check.passed ? "passed" : "failed"}
                            </span>
                        </div>

                        {testReport.auth_check.failure && (
                            <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[var(--error)]">
                                {testReport.auth_check.failure.message}
                            </div>
                        )}
                        {testReport.connections_open_check.failure && (
                            <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[var(--error)]">
                                {testReport.connections_open_check.failure.message}
                            </div>
                        )}

                        {testReport.identity && (
                            <div className="flex flex-wrap gap-[16px] text-[var(--modal-text-primary)]">
                                <span><span className="text-[var(--modal-text-tertiary)]">Workspace: </span>{testReport.identity.team_name}</span>
                                <span><span className="text-[var(--modal-text-tertiary)]">Bot: </span>{testReport.identity.bot_handle}</span>
                            </div>
                        )}

                        <div className="flex flex-col gap-[4px]">
                            <span className="text-[var(--modal-text-tertiary)]">Scopes</span>
                            <ul className="flex flex-col gap-[2px]">
                                {testReport.scopes.map((s) => (
                                    <li key={s.scope} className={`inline-flex items-center gap-[5px] font-mono ${s.granted ? "text-green-600" : "text-[var(--error)]"}`}>
                                        {s.granted ? <Check className="w-[11px] h-[11px]" /> : <X className="w-[11px] h-[11px]" />}
                                        {s.scope}
                                    </li>
                                ))}
                            </ul>
                        </div>
                    </div>
                )}
            </div>

            {removeError && (
                <div className="px-[10px] py-[8px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[12px] text-[var(--error)]">
                    {removeError}
                </div>
            )}
            {status && (
                <div>
                    <button
                        type="button"
                        onClick={handleRemove}
                        disabled={removing}
                        className="h-[30px] px-[12px] rounded-[8px] text-[12px] font-medium text-[#E01E5A] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[6px]"
                    >
                        {removing && <Loader2 className="w-[12px] h-[12px] animate-spin" />}
                        {removing ? "Removing…" : "Remove Slack channel"}
                    </button>
                </div>
            )}
        </div>
    );
});

// ─── small helpers ─────────────────────────────────────────────────────────────

/** "expires in Xm" for a still-valid pairing code, or null once it's expired
 *  (the panel then falls back to offering a fresh "Generate pairing code"). */
function formatPairingExpiry(expiresAtUnix: number): string | null {
    const secondsLeft = expiresAtUnix - Math.floor(Date.now() / 1000);
    if (secondsLeft <= 0) return null;
    return `expires in ${Math.max(1, Math.ceil(secondsLeft / 60))}m`;
}

function DirInput({ id, value, onChange, placeholder }: { id: string; value: string; onChange: (v: string) => void; placeholder?: string }) {
    return (
        <div className="relative">
            <input
                id={id} type="text" value={value}
                onChange={(e) => onChange(e.target.value)} placeholder={placeholder}
                autoCorrect="off" autoCapitalize="off" spellCheck={false}
                className="w-full h-[43px] px-[12px] pr-[40px] rounded-[12px] border-[1.5px] border-[color-mix(in_srgb,var(--modal-border-secondary)_55%,var(--modal-text-tertiary)_45%)] text-[16px] font-mono text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_4px_color-mix(in_srgb,var(--modal-accent)_22%,transparent)] transition-all"
            />
            <button
                type="button"
                onClick={async () => {
                    const selected = await invoke<string | null>("pick_directory");
                    if (selected) onChange(selected);
                }}
                className="absolute right-[6px] top-1/2 -translate-y-1/2 w-[28px] h-[28px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
                aria-label="Browse for directory"
            >
                <FolderOpen className="w-[15px] h-[15px]" />
            </button>
        </div>
    );
}

/** Section-title chip: a small self-bordered rounded rectangle sitting fully
 *  INSIDE its own section, offset from the section's top/left edges by a
 *  clear margin. Not a fieldset-legend tab — it never straddles the outer
 *  border or the divider between sections (see the Instructions tab), so
 *  neither line ever cuts through it.
 *
 *  The chip's own text always stays full-contrast (--modal-text-label) —
 *  the fill (via `group`/`group-focus-within` on its wrapping section div)
 *  is what signals which section currently has focus, so the label text
 *  itself doesn't need a second idle/focused state on top of that. The
 *  idle-vs-focused dimming instead lives on the field's own input text (see
 *  the `dim` prop on AutoGrowTextarea, driven by the parent's
 *  `focusedSection` state) so the *other* section reads as dimmed once one
 *  box has focus — not the chip, and not either box while neither has
 *  focus yet. */
function FloatingBoxLabel({ htmlFor, children }: { htmlFor: string; children: React.ReactNode }) {
    return (
        <label
            htmlFor={htmlFor}
            className="absolute top-[12px] left-[12px] px-[14px] py-[6px] rounded-[8px] border border-[color-mix(in_srgb,var(--modal-border-secondary)_55%,var(--modal-text-tertiary)_45%)] text-[14px] font-semibold text-[var(--modal-text-label)] leading-[17px] select-none transition-colors group-focus-within:bg-[var(--modal-accent)] group-focus-within:border-[var(--modal-accent)] group-focus-within:text-white"
        >
            {children}
        </label>
    );
}

/** Mousedown handler for a Persona/Special Instructions section wrapper: a
 *  click anywhere inside the section — its padding, the gap around the
 *  title chip, empty space below the text — should focus that section's
 *  textarea. Direct clicks on the textarea or the chip (a native
 *  `<label htmlFor>`) already focus correctly on their own, so those are
 *  left alone. Everything else inside the section is plain non-focusable
 *  `<div>` — the browser's default mousedown behavior there blurs whatever
 *  was previously focused and focuses nothing, which is what made clicking
 *  near (but not exactly on) the header read as "unfocusing" the field even
 *  though the click never left the section. */
function focusSectionOnMouseDown(e: React.MouseEvent<HTMLDivElement>) {
    const target = e.target as HTMLElement;
    if (target.closest("textarea") || target.closest("label")) return;
    e.preventDefault();
    e.currentTarget.querySelector("textarea")?.focus();
}

/** Borderless textarea that grows to fit its own content independently of any
 *  sibling field, starting from a shared `minHeight` and growing unbounded —
 *  no internal max-height or scrollbar of its own. The modal's content area
 *  is already the single outer scroll container (see the Instructions tab),
 *  so a field never needs its own inner scrollbar on top of that. Pairs with
 *  a container that supplies the border (see the Instructions tab's Persona/
 *  Special Instructions boxes) — this component only owns the text surface
 *  and its height.
 *
 *  `dim` is driven by the parent (see `focusedSection` in the Instructions
 *  tab), not by this field's own focus state — dimming here is *relative*:
 *  a box only dims while its sibling has focus, and neither dims when
 *  nothing in the pair has focus yet (e.g. right after the tab opens). */
function AutoGrowTextarea({ id, value, onChange, placeholder, minHeight = 104, dim = false }: {
    id: string; value: string; onChange: (v: string) => void; placeholder?: string;
    minHeight?: number; dim?: boolean;
}) {
    const ref = useRef<HTMLTextAreaElement>(null);

    useLayoutEffect(() => {
        const el = ref.current;
        if (!el) return;
        el.style.height = "auto";
        el.style.height = `${Math.max(el.scrollHeight, minHeight)}px`;
    }, [value, minHeight]);

    return (
        <textarea
            ref={ref}
            id={id}
            value={value}
            onChange={(e) => onChange(e.target.value)}
            placeholder={placeholder}
            autoCorrect="off" autoCapitalize="off" spellCheck={false}
            style={{ minHeight }}
            className={`w-full bg-transparent text-[16px] placeholder:text-[var(--modal-text-tertiary)] outline-none resize-none leading-relaxed transition-[height,color] duration-100 ease-out ${dim ? "text-[var(--modal-text-secondary)]" : "text-[var(--modal-text-primary)]"}`}
        />
    );
}

