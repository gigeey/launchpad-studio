import { useEffect, useId, useRef, useState, useCallback } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { ChevronDown, X } from "lucide-react";
import { useChatStore } from "../../stores/chatStore";
import { agentAvatarColor } from "../../lib/agentColors";
import { useIsDark } from "../../stores/userPreferencesStore";
import { resolveAgent } from "../../lib/agents/resolveAgent";
import * as api from "../../lib/api";
import { AgentProfile } from "../../types/api";

// ---------------------------------------------------------------------------
// Agent card shared by coordinator and members
// ---------------------------------------------------------------------------
interface AgentCardProps {
  agentId: string;
  name: string;
  emoji?: string;
  description?: string;
  role?: string;
  isCoordinator?: boolean;
  isExpanded?: boolean;
  isDark: boolean;
  onClick?: () => void;
}

function AgentCard({ name, emoji, description, role, isCoordinator, isExpanded, isDark, onClick }: AgentCardProps) {
  return (
    <div
      className={`flex gap-3 p-[14px] rounded-[14px] border transition-colors cursor-pointer shadow ${
        isExpanded
          ? "border-[var(--accent)] bg-[var(--bg-secondary)]"
          : "border-[var(--border-secondary)] bg-[var(--bg-secondary)] hover:border-[var(--border-primary)]"
      }`}
      onClick={onClick}
    >
      <div
        className="w-[40px] h-[40px] rounded-[12px] flex items-center justify-center text-[20px] flex-shrink-0"
        style={{ backgroundColor: agentAvatarColor(name, isDark) }}
      >
        {emoji ?? name.charAt(0).toUpperCase()}
      </div>
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-[6px]">
          <span className="font-semibold text-[14px] text-[var(--text-primary)] truncate">
            {name}
          </span>
          {isCoordinator && (
            <span className="text-[10px] font-medium px-[6px] py-[1px] rounded-full bg-blue-500/15 text-blue-500 flex-shrink-0">
              coordinator
            </span>
          )}
        </div>
        <div
          className={`text-[12px] line-clamp-1 mt-[2px] ${
            description
              ? "text-[var(--text-secondary)]"
              : "text-[var(--text-tertiary)] italic"
          }`}
        >
          {description || "No description"}
        </div>
        {role && (
          <div className="mt-1 text-[12px] text-[var(--text-secondary)] bg-[var(--bg-tertiary)] px-[10px] py-[2px] rounded-[8px] border border-[var(--border-secondary)]">

            <span className="line-clamp-1">{role}</span>
          </div>
        )}
      </div>
      <ChevronDown
        className={`w-[16px] h-[16px] text-[var(--text-tertiary)] flex-shrink-0 mt-[2px] transition-transform ${
          isExpanded ? "rotate-180" : ""
        }`}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Read-only agent settings modal
// ---------------------------------------------------------------------------
type ModalSection = "overview" | "configuration" | "capabilities" | "purpose";

function AgentSettingsModal({
  open,
  profile,
  displayName,
  emoji,
  roleDescription,
  purpose,
  isCoordinator,
  isDark,
  onClose,
}: {
  open: boolean;
  profile: AgentProfile | null;
  displayName: string;
  emoji?: string;
  roleDescription?: string;
  purpose?: string;
  isCoordinator: boolean;
  isDark: boolean;
  onClose: () => void;
}) {
  const titleId = useId();
  const [section, setSection] = useState<ModalSection>("overview");

  // Reset to the first section whenever the modal opens or the shown agent
  // changes, so the user always lands on Overview for a freshly-opened agent.
  useEffect(() => {
    if (!open) setSection("overview");
  }, [open]);
  useEffect(() => {
    setSection("overview");
  }, [profile?.id]);

  // Escape closes the modal.
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open, onClose]);

  const tabs: { id: ModalSection; label: string; visible: boolean }[] = profile
    ? [
        { id: "overview", label: "Overview", visible: true },
        { id: "configuration", label: "Configuration", visible: true },
        {
          id: "capabilities",
          label: "Capabilities",
          visible:
            profile.skills.length > 0 ||
            (profile.tools?.allow.length ?? 0) > 0,
        },
        {
          id: "purpose",
          label: "When to Use",
          visible: !!purpose,
        },
      ]
    : [];

  return (
    <AnimatePresence>
      {open && profile && (
        <div className="fixed inset-0 z-[300] flex items-center justify-center">
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
            className="absolute inset-0 bg-black/40"
            onClick={onClose}
          />
          <motion.div
            role="dialog"
            aria-modal="true"
            aria-labelledby={titleId}
            initial={{ opacity: 0, scale: 0.96 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.96 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="relative w-full max-w-[560px] h-[640px] max-h-[85vh] rounded-[16px] overflow-hidden bg-[var(--bg-primary)] border border-[var(--border-secondary)] flex flex-col shadow-2xl"
            style={{
              boxShadow:
                "0 0 0 1px rgba(0,0,0,0.13), 0 24px 64px 0 rgba(0,0,0,0.35)",
            }}
          >
            {/* Header */}
            <div className="flex items-center gap-[12px] px-[20px] py-[16px] border-b border-[var(--border-secondary)]">
              <div
                className="w-[44px] h-[44px] rounded-[12px] flex items-center justify-center text-[22px] flex-shrink-0"
                style={{ backgroundColor: agentAvatarColor(displayName, isDark) }}
              >
                {emoji ?? displayName.charAt(0).toUpperCase()}
              </div>
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-[6px]">
                  <span
                    id={titleId}
                    className="font-semibold text-[15px] text-[var(--text-primary)] truncate"
                  >
                    {displayName}
                  </span>
                  {isCoordinator && (
                    <span className="text-[10px] font-medium px-[6px] py-[1px] rounded-full bg-blue-500/15 text-blue-500 flex-shrink-0">
                      coordinator
                    </span>
                  )}
                </div>
                {profile.description && (
                  <div className="text-[12px] text-[var(--text-secondary)] mt-[2px] line-clamp-1">
                    {profile.description}
                  </div>
                )}
              </div>
              <button
                type="button"
                onClick={onClose}
                className="w-[32px] h-[32px] rounded-[8px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] transition-colors cursor-pointer flex-shrink-0"
                aria-label="Close"
              >
                <X className="w-[18px] h-[18px]" />
              </button>
            </div>

            {/* Tab nav */}
            <div className="flex items-center gap-[4px] px-[12px] pt-[10px] border-b border-[var(--border-secondary)]">
              {tabs
                .filter((t) => t.visible)
                .map((t) => {
                  const active = section === t.id;
                  return (
                    <button
                      key={t.id}
                      type="button"
                      onClick={() => setSection(t.id)}
                      className={`relative px-[12px] py-[8px] text-[12px] font-medium rounded-t-[8px] transition-colors cursor-pointer ${
                        active
                          ? "text-[var(--text-primary)]"
                          : "text-[var(--text-tertiary)] hover:text-[var(--text-secondary)]"
                      }`}
                    >
                      {t.label}
                      {active && (
                        <span className="absolute left-[8px] right-[8px] -bottom-px h-[2px] rounded-t-full bg-[var(--accent)]" />
                      )}
                    </button>
                  );
                })}
            </div>

            {/* Body */}
            <div className="flex-1 min-h-0 overflow-y-auto px-[20px] py-[18px]">
              {section === "overview" && (
                <div className="space-y-[14px]">
                  <SettingsRow
                    label="Name"
                    value={displayName}
                  />
                  {profile.description && (
                    <SettingsRow label="Description" value={profile.description} />
                  )}
                  {roleDescription && (
                    <SettingsRow
                      label="Role on Team"
                      value={roleDescription}
                    />
                  )}
                  {profile.template && (
                    <SettingsRow
                      label="Template"
                      value={profile.template}
                    />
                  )}
                </div>
              )}

              {section === "configuration" && (
                <div className="space-y-[20px]">
                  {/* Kind + Template */}
                  <div className="rounded-[12px] border border-[var(--border-secondary)] bg-[var(--bg-tertiary)] px-[14px] py-[12px] flex flex-col gap-[10px]">
                    <div className="flex items-center justify-between gap-[12px]">
                      <span className="text-[11px] font-medium text-[var(--text-tertiary)] uppercase tracking-wide">
                        Kind
                      </span>
                      <span className="text-[12px] font-medium text-[var(--text-primary)]">
                        {profile.provider?.type
                          ? profile.provider.type.toUpperCase()
                          : "CLI"}
                      </span>
                    </div>
                    <div className="flex items-center justify-between gap-[12px]">
                      <span className="text-[11px] font-medium text-[var(--text-tertiary)] uppercase tracking-wide">
                        Template
                      </span>
                      <span className="text-[12px] font-medium text-[var(--text-primary)] font-mono">
                        {profile.template ?? "Custom"}
                      </span>
                    </div>
                  </div>

                  {/* Provider */}
                  <SectionDivider>Provider</SectionDivider>
                  <SettingsRow
                    label="Command"
                    value={profile.provider?.command || "—"}
                    monospace
                  />
                  <ListRow
                    label="Args"
                    items={profile.provider?.args ?? []}
                  />
                  <div className="grid grid-cols-2 gap-[12px]">
                    <SettingsRow
                      label="Output Format"
                      value={profile.provider?.output_format || "—"}
                    />
                    <SettingsRow
                      label="Input Mode"
                      value={profile.provider?.input_mode || "—"}
                    />
                  </div>
                  <SettingsRow
                    label="Normalizer"
                    value={profile.provider?.normalizer || "(auto-detect)"}
                    monospace
                    muted={!profile.provider?.normalizer}
                  />
                  <SettingsRow
                    label="System Prompt Arg"
                    value={profile.provider?.system_prompt_arg || "—"}
                    monospace
                    muted={!profile.provider?.system_prompt_arg}
                  />
                  <SettingsRow
                    label="Model Arg"
                    value={profile.provider?.model_arg || "—"}
                    monospace
                    muted={!profile.provider?.model_arg}
                  />
                  <SettingsRow
                    label="Session Arg"
                    value={profile.provider?.session_arg || "—"}
                    monospace
                    muted={!profile.provider?.session_arg}
                  />
                  <ListRow
                    label="Resume Args"
                    items={profile.provider?.resume_args ?? []}
                  />
                  <KVRow
                    label="Model Aliases"
                    values={profile.provider?.model_aliases ?? {}}
                  />
                  <SettingsRow
                    label="Model"
                    value={profile.model || "(provider default)"}
                    monospace
                    muted={!profile.model}
                  />

                  {/* Runtime */}
                  <SectionDivider>Runtime</SectionDivider>
                  <div className="grid grid-cols-2 gap-[12px]">
                    <SettingsRow
                      label="Timeout (s)"
                      value={String(profile.timeout_seconds)}
                      monospace
                    />
                    <SettingsRow
                      label="No-output Timeout (ms)"
                      value={String(profile.provider?.no_output_timeout_ms ?? 0)}
                      monospace
                    />
                  </div>
                  <SettingsRow
                    label="Max Instances"
                    value={String(profile.max_instances)}
                    monospace
                  />
                  <div className="flex items-center justify-between">
                    <div>
                      <p className="text-[13px] font-medium text-[var(--text-primary)]">
                        Clear Environment
                      </p>
                      <p className="text-[11px] text-[var(--text-tertiary)]">
                        Start the process with a clean env
                      </p>
                    </div>
                    <span
                      className={`text-[11px] font-medium px-[8px] py-[2px] rounded-full ${
                        profile.provider?.clear_env
                          ? "bg-[var(--accent)]/15 text-[var(--accent)]"
                          : "bg-[var(--bg-tertiary)] text-[var(--text-tertiary)]"
                      }`}
                    >
                      {profile.provider?.clear_env ? "On" : "Off"}
                    </span>
                  </div>
                  <KVRow
                    label="Environment Variables"
                    values={profile.env ?? {}}
                  />
                  <SettingsRow
                    label="Serialize"
                    value={profile.serialize ? "Yes" : "No"}
                  />
                  {profile.working_dir && (
                    <SettingsRow
                      label="Working Directory"
                      value={profile.working_dir}
                      monospace
                    />
                  )}
                </div>
              )}

              {section === "capabilities" && (
                <div className="space-y-[16px]">
                  {profile.skills.length > 0 && (
                    <ChipGroup label="Skills" items={profile.skills} />
                  )}
                  {profile.tools && profile.tools.allow.length > 0 && (
                    <ChipGroup
                      label="Tools (allow)"
                      items={profile.tools.allow}
                    />
                  )}
                  {profile.tools && profile.tools.deny.length > 0 && (
                    <ChipGroup
                      label="Tools (deny)"
                      items={profile.tools.deny}
                    />
                  )}
                </div>
              )}

              {section === "purpose" && purpose && (
                <p className="text-[13px] text-[var(--text-primary)] leading-[1.6] whitespace-pre-wrap break-words">
                  {purpose}
                </p>
              )}
            </div>
          </motion.div>
        </div>
      )}
    </AnimatePresence>
  );
}

function SettingsRow({
  label,
  value,
  monospace,
  muted,
}: {
  label: string;
  value: string;
  monospace?: boolean;
  muted?: boolean;
}) {
  return (
    <div>
      <span className="text-[11px] font-medium text-[var(--text-tertiary)] uppercase tracking-wide">
        {label}
      </span>
      <p
        className={`text-[13px] mt-[4px] whitespace-pre-wrap break-words ${
          monospace ? "font-mono" : ""
        } ${muted ? "text-[var(--text-tertiary)] italic" : "text-[var(--text-primary)]"}`}
      >
        {value}
      </p>
    </div>
  );
}

function ListRow({ label, items }: { label: string; items: string[] }) {
  return (
    <div>
      <span className="text-[11px] font-medium text-[var(--text-tertiary)] uppercase tracking-wide">
        {label}
      </span>
      {items.length === 0 ? (
        <p className="text-[13px] text-[var(--text-tertiary)] italic mt-[4px]">—</p>
      ) : (
        <div className="flex flex-wrap gap-[4px] mt-[6px]">
          {items.map((s, i) => (
            <span
              key={`${s}-${i}`}
              className="text-[11px] font-mono px-[8px] py-[3px] rounded-[6px] bg-[var(--bg-tertiary)] text-[var(--text-secondary)] border border-[var(--border-secondary)]"
            >
              {s}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

function KVRow({
  label,
  values,
}: {
  label: string;
  values: Record<string, string>;
}) {
  const entries = Object.entries(values);
  return (
    <div>
      <span className="text-[11px] font-medium text-[var(--text-tertiary)] uppercase tracking-wide">
        {label}
      </span>
      {entries.length === 0 ? (
        <p className="text-[13px] text-[var(--text-tertiary)] italic mt-[4px]">—</p>
      ) : (
        <div className="mt-[6px] rounded-[8px] border border-[var(--border-secondary)] divide-y divide-[var(--border-secondary)] overflow-hidden">
          {entries.map(([k, v]) => (
            <div
              key={k}
              className="flex items-center gap-[10px] px-[10px] py-[6px] bg-[var(--bg-tertiary)]"
            >
              <span className="text-[12px] font-mono text-[var(--text-primary)] flex-shrink-0">
                {k}
              </span>
              <span className="text-[var(--text-tertiary)]">→</span>
              <span className="text-[12px] font-mono text-[var(--text-secondary)] truncate">
                {v}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function SectionDivider({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex items-center gap-[10px] pt-[2px]">
      <p className="text-[12px] font-semibold text-[var(--text-primary)] uppercase tracking-wide">
        {children}
      </p>
      <div className="flex-1 h-px bg-[var(--border-secondary)]" />
    </div>
  );
}

function ChipGroup({ label, items }: { label: string; items: string[] }) {
  return (
    <div>
      <span className="text-[11px] font-medium text-[var(--text-tertiary)] uppercase tracking-wide">
        {label}
      </span>
      <div className="flex flex-wrap gap-[4px] mt-[8px]">
        {items.map((s) => (
          <span
            key={s}
            className="text-[11px] px-[8px] py-[3px] rounded-full bg-[var(--bg-tertiary)] text-[var(--text-secondary)] border border-[var(--border-secondary)]"
          >
            {s}
          </span>
        ))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// SVG connector lines
// ---------------------------------------------------------------------------
function ConnectorLines({
  containerRef,
  coordinatorRef,
  memberRefs,
  memberSides,
}: {
  containerRef: React.RefObject<HTMLDivElement | null>;
  coordinatorRef: React.RefObject<HTMLDivElement | null>;
  memberRefs: React.RefObject<Map<string, HTMLDivElement>>;
  memberSides: Map<string, "left" | "right">;
}) {
  const [paths, setPaths] = useState<string[]>([]);

  const compute = useCallback(() => {
    const container = containerRef.current;
    const coord = coordinatorRef.current;
    const refs = memberRefs.current;
    if (!container || !coord || !refs) return;

    const cRect = container.getBoundingClientRect();
    const coordRect = coord.getBoundingClientRect();

    const newPaths: string[] = [];
    refs.forEach((el, id) => {
      const side = memberSides.get(id) ?? "left";
      const r = el.getBoundingClientRect();

      const startX =
        side === "left"
          ? coordRect.left - cRect.left
          : coordRect.right - cRect.left;
      const startY =
        coordRect.top + coordRect.height / 2 - cRect.top;

      const endX =
        side === "left"
          ? r.left - cRect.left
          : r.right - cRect.left;
      const endY = r.top + r.height / 2 - cRect.top;

      const padding = 32;
      const cpX =
        side === "left"
          ? Math.min(startX, endX) - padding
          : Math.max(startX, endX) + padding;

      const radius = 12;
      const dirStartX = Math.sign(cpX - startX);
      const dirY = Math.sign(endY - startY) || 1;
      const dirEndX = Math.sign(endX - cpX);

      const safeRadius = Math.min(
        radius,
        Math.abs(cpX - startX) / 2,
        Math.abs(endY - startY) / 2,
        Math.abs(endX - cpX) / 2
      );

      const p = [
        `M ${startX} ${startY}`,
        `L ${cpX - dirStartX * safeRadius} ${startY}`,
        `Q ${cpX} ${startY} ${cpX} ${startY + dirY * safeRadius}`,
        `L ${cpX} ${endY - dirY * safeRadius}`,
        `Q ${cpX} ${endY} ${cpX + dirEndX * safeRadius} ${endY}`,
        `L ${endX} ${endY}`,
      ].join(" ");

      newPaths.push(p);
    });
    setPaths(newPaths);
  }, [containerRef, coordinatorRef, memberRefs, memberSides]);

  useEffect(() => {
    compute();
    window.addEventListener("resize", compute);
    return () => window.removeEventListener("resize", compute);
  }, [compute]);

  useEffect(() => {
    const t = setTimeout(compute, 50);
    return () => clearTimeout(t);
  }, [compute]);

  if (paths.length === 0) return null;

  return (
    <svg className="absolute inset-0 pointer-events-none" style={{ overflow: "visible" }}>
      {paths.map((d, i) => (
        <path
          key={i}
          d={d}
          fill="none"
          stroke="var(--border-secondary)"
          strokeWidth="2"
        />
      ))}
    </svg>
  );
}

// ---------------------------------------------------------------------------
// AgentOrgTree — coordinator card + member grid + connector lines + settings modal
// Accepts any coordinator/members shape; aliasMap overrides display names.
// ---------------------------------------------------------------------------
export function AgentOrgTree({
  coordinatorId,
  members,
  aliasMap = {},
}: {
  coordinatorId: string | null;
  members: Array<{ agent_id: string; role_description: string }>;
  aliasMap?: Record<string, string>;
}) {
  const isDark = useIsDark();
  const agentSnapshots = useChatStore((s) => s.agents);

  const containerRef = useRef<HTMLDivElement>(null);
  const coordinatorRef = useRef<HTMLDivElement>(null);
  const memberRefsMap = useRef<Map<string, HTMLDivElement>>(new Map());

  const [agentProfiles, setAgentProfiles] = useState<Record<string, AgentProfile>>({});
  const [expandedAgentId, setExpandedAgentId] = useState<string | null>(null);

  useEffect(() => {
    const ids = [
      ...(coordinatorId ? [coordinatorId] : []),
      ...members.map(m => m.agent_id),
    ];
    const missing = ids.filter(id => !agentProfiles[id]);
    if (missing.length === 0) return;
    Promise.all(missing.map(id => api.getAgent(id).catch(() => null)))
      .then(profiles => {
        const newMap = { ...agentProfiles };
        profiles.forEach(p => { if (p) newMap[p.id] = p; });
        setAgentProfiles(newMap);
      })
      .catch(console.error);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [coordinatorId, members]);

  const childIds = members
    .map((m) => m.agent_id)
    .filter((id) => id !== coordinatorId);

  const memberRoles = Object.fromEntries(
    members.map((m) => [m.agent_id, m.role_description]),
  );

  const leftCol: string[] = [];
  const rightCol: string[] = [];
  childIds.forEach((id, i) => {
    if (i % 2 === 0) leftCol.push(id);
    else rightCol.push(id);
  });

  const memberSides = new Map<string, "left" | "right">();
  leftCol.forEach((id) => memberSides.set(id, "left"));
  rightCol.forEach((id) => memberSides.set(id, "right"));

  // Prefer aliasMap override, then fetched AgentProfile, then resolveAgent result.
  // The coordinator's id often isn't in the AgentSnapshot list (snapshots track
  // recent activity), so without the profile fallback the card renders a
  // truncated id like "f562989d…".
  const displayFor = (id: string) => {
    const resolved = resolveAgent(id, agentSnapshots);
    const profile = agentProfiles[id];
    const alias = aliasMap[id];
    const displayName = alias ?? (resolved.isFallback && profile ? profile.name : resolved.displayName);
    const emoji = (resolved.isFallback && profile) ? (profile.emoji ?? resolved.emoji) : resolved.emoji;
    return { displayName, emoji };
  };

  const handleAgentClick = (agentId: string) => {
    setExpandedAgentId((prev) => (prev === agentId ? null : agentId));
  };

  const activeProfile = expandedAgentId ? agentProfiles[expandedAgentId] ?? null : null;
  const activeDisplay = expandedAgentId ? displayFor(expandedAgentId) : null;

  // "When to use" comes from the coordinator's own address book (delegates_to),
  // not the target's system prompt — it's the coordinator's stated reason for
  // reaching this agent. Only shown when that entry exists with real text.
  const coordinatorProfile = coordinatorId ? agentProfiles[coordinatorId] : undefined;
  const activePurpose = expandedAgentId
    ? coordinatorProfile?.delegates_to?.find(
        (d) => d.target_agent_id === expandedAgentId,
      )?.purpose?.trim() || undefined
    : undefined;

  return (
    <div ref={containerRef} className="relative max-w-[700px] mx-auto">
      <ConnectorLines
        containerRef={containerRef}
        coordinatorRef={coordinatorRef}
        memberRefs={memberRefsMap}
        memberSides={memberSides}
      />

      {coordinatorId && (() => {
        const { displayName, emoji } = displayFor(coordinatorId);
        return (
          <div ref={coordinatorRef} className="mx-auto max-w-[400px] mb-[40px]">
            <AgentCard
              agentId={coordinatorId}
              name={displayName}
              emoji={emoji}
              description={agentProfiles[coordinatorId]?.description}
              role={memberRoles[coordinatorId]}
              isCoordinator
              isExpanded={expandedAgentId === coordinatorId}
              isDark={isDark}
              onClick={() => handleAgentClick(coordinatorId)}
            />
          </div>
        );
      })()}

      {childIds.length > 0 && (
        <div className="grid grid-cols-2 gap-x-[40px] gap-y-[16px]">
          {Array.from({ length: Math.max(leftCol.length, rightCol.length) }).map(
            (_, rowIdx) => {
              const leftId = leftCol[rowIdx];
              const rightId = rightCol[rowIdx];
              return [
                <div
                  key={`l-${rowIdx}`}
                  ref={(el) => {
                    if (leftId && el) memberRefsMap.current.set(leftId, el);
                  }}
                >
                  {leftId && (() => {
                    const { displayName, emoji } = displayFor(leftId);
                    return (
                      <AgentCard
                        agentId={leftId}
                        name={displayName}
                        emoji={emoji}
                        description={agentProfiles[leftId]?.description}
                        role={memberRoles[leftId]}
                        isExpanded={expandedAgentId === leftId}
                        isDark={isDark}
                        onClick={() => handleAgentClick(leftId)}
                      />
                    );
                  })()}
                </div>,
                <div
                  key={`r-${rowIdx}`}
                  ref={(el) => {
                    if (rightId && el) memberRefsMap.current.set(rightId, el);
                  }}
                >
                  {rightId && (() => {
                    const { displayName, emoji } = displayFor(rightId);
                    return (
                      <AgentCard
                        agentId={rightId}
                        name={displayName}
                        emoji={emoji}
                        description={agentProfiles[rightId]?.description}
                        role={memberRoles[rightId]}
                        isExpanded={expandedAgentId === rightId}
                        isDark={isDark}
                        onClick={() => handleAgentClick(rightId)}
                      />
                    );
                  })()}
                </div>,
              ];
            },
          )}
        </div>
      )}

      <AgentSettingsModal
        open={expandedAgentId !== null && activeProfile !== null}
        profile={activeProfile}
        displayName={activeDisplay?.displayName ?? ""}
        emoji={activeDisplay?.emoji}
        roleDescription={expandedAgentId ? memberRoles[expandedAgentId] : undefined}
        purpose={activePurpose}
        isCoordinator={expandedAgentId === coordinatorId}
        isDark={isDark}
        onClose={() => setExpandedAgentId(null)}
      />
    </div>
  );
}
