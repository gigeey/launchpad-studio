use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::attachment::FileCapability;

pub type AgentId = String;

/// Which runner implementation drives this agent's execution.
///
/// This is a creation-time, immutable property of an agent profile.
/// Existing profiles that pre-date this field deserialize as `Cli` via
/// `#[serde(default)]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRunnerMode {
    /// Existing CLI spawn path. Default for backwards compatibility.
    #[default]
    Cli,
    /// In-process API runner. Drives `query_loop::run_session` against
    /// whichever provider the agent's `provider` field resolves to.
    Api,
}

/// An entry in an agent's address book of delegate targets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegateTarget {
    pub target_agent_id: AgentId,
    pub name: String,
    pub purpose: String,
    #[serde(default)]
    pub share_context_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(from = "AgentProfileWire")]
pub struct AgentProfile {
    pub id: AgentId,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub emoji: Option<String>,
    pub provider: ProviderConfig,
    pub model: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    pub system_prompt: Option<String>,
    pub tools: Option<ToolsConfig>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default = "default_max_instances")]
    pub max_instances: u32,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Per-agent cap on the number of model-completion turns a single native
    /// (in-process API) run may take before the runner force-stops it — the
    /// same class of safety rail `timeout_seconds` is, but bounding turn
    /// count instead of wall-clock time. `None` defers to
    /// [`DEFAULT_MAX_TURNS`]. Profiles predating this field deserialise as
    /// `None` via `#[serde(default)]`, so every already-persisted profile
    /// keeps following whatever `DEFAULT_MAX_TURNS` currently resolves to
    /// rather than getting a value frozen in at migration time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Optional custom home directory for agent-level config (skills, rules, instructions).
    /// If not set, defaults to `~/.launchpad_studio/agent_homes/{id}/`.
    #[serde(default)]
    pub home_dir: Option<String>,
    #[serde(default = "default_serialize")]
    pub serialize: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflows: Option<WorkflowBinding>,
    /// Which preset template the agent was created from (e.g. `"claude"`, `"cursor"`, `"codex"`).
    /// `None` means a fully custom configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Which runner implementation drives this agent. Locked at creation time.
    /// Profiles without this field (pre-Loop-F) load as `Cli` via `#[serde(default)]`.
    #[serde(default)]
    pub runner_mode: AgentRunnerMode,
    /// Per-agent enablement map for globally-installed plugins, keyed by plugin name.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub enabled_plugins: std::collections::HashMap<String, PluginEnablement>,
    /// Names of enabled convention-folder skills sourced from the global
    /// `<data_root>/.launchpad/skills` directory. These are untrusted,
    /// human-dropped folders (separate from the self-improvement skill pool),
    /// so enablement is explicit opt-in: `None`/empty means none are enabled —
    /// unlike `PluginEnablement::enabled_skills`, absence never means "all".
    /// Profiles predating this field deserialize as `None` via `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_launchpad_global_skills: Option<Vec<String>>,
    /// Per-project enablement of convention-folder skills sourced from
    /// `<focus_path>/.launchpad/skills`. Keyed by the canonicalized project
    /// path (see [`canonical_project_key`]) so enablement survives
    /// re-focusing and is shared across threads pointed at the same project.
    /// Same untrusted-by-default, explicit-opt-in semantics as
    /// `enabled_launchpad_global_skills`: an absent key means no skills from
    /// that project are enabled. Profiles predating this field deserialize as
    /// an empty map via `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub enabled_launchpad_project_skills: std::collections::BTreeMap<String, Vec<String>>,
    /// If set, the agent is an inline coordinator owned by the given team and
    /// should be hidden from chat-surface agent lists/search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owning_team_id: Option<String>,
    /// Selects the native (in-process API) provider. Relevant only when
    /// `runner_mode = Api`. Absent defaults to `Anthropic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_provider: Option<NativeProvider>,
    /// Provider-neutral reasoning channel configuration. Absent (`None`)
    /// preserves whatever default the provider's spawn layer would have used
    /// without this field — for the Claude CLI that's adaptive thinking with
    /// `display = "omitted"`, which is silent on the wire. To make the model's
    /// reasoning visible mid-stream the caller must set this to e.g.
    /// `ThinkingConfig { mode: Adaptive, display: Summarized, .. }`.
    ///
    /// Per-provider mapping lives in the corresponding spawn layer — the
    /// profile layer carries the canonical shape only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// Per-agent override for the maximum number of tokens the provider may
    /// spend generating its response. Relevant only when `runner_mode =
    /// Api`. Resolved with the same precedence as [`AgentProfile::model`]:
    /// this override, then the provider's persisted `providers.toml` value,
    /// then that provider request builder's hardcoded fallback. `None` here
    /// does not mean "unbounded" — it means "defer to the next tier".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Per-agent override for how much conversation history (in an
    /// approximate token count) the native runner keeps in context before
    /// trimming the oldest turns. Neither Anthropic's Messages API nor
    /// OpenAI's Chat Completions API expose a "cap total context tokens"
    /// request parameter — both only cap *output* tokens — so this budget
    /// is enforced client-side against `CompletionRequest.messages` before
    /// the request ever reaches the wire (see
    /// `ao_engine_tools_runner::message::truncate_to_context_budget`).
    /// Same per-agent ?? persisted-config ?? provider-default precedence as
    /// [`AgentProfile::model`]; `None` means "no cap enforced".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u32>,
    /// Per-agent override for the native (API) reasoning-effort level. See
    /// [`ReasoningEffort`] for why this is an ordinal level rather than a
    /// raw token budget, and how it relates to the older [`ThinkingConfig`]
    /// mechanism above. Same per-agent ?? persisted-config ?? provider-default
    /// precedence as [`AgentProfile::model`]. `None` defers to the next tier;
    /// providers with no reasoning channel at all ignore the resolved value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Address book of agents this profile may delegate work to.
    /// Absent in pre-H profiles; deserialises as empty vec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegates_to: Vec<DelegateTarget>,
    /// The agent's identity, voice, and expertise description authored by the user.
    /// Rendered as Section 4a in the canonical system prompt.
    /// Absent in pre-composer profiles; deserialises as None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// Behavior rules and do's/don'ts authored by the user.
    /// Rendered as Section 4b in the canonical system prompt.
    /// Absent in pre-composer profiles; deserialises as None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub special_instructions: Option<String>,
    /// Archival copy of the raw system_prompt before migration.
    /// Populated by the one-shot migrator; retained for one release cycle.
    /// Absent in profiles that have not been migrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_system_prompt: Option<String>,
    /// Per-profile cap on spawn/delegation depth. When absent, the resolver
    /// `effective_depth_cap` falls back to the global `DEFAULT_DEPTH_CAP`
    /// (subagent spawner) or `DELEGATE_DEPTH_CAP` (Delegate tool).
    /// Profiles authored before this field existed deserialise cleanly with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delegation_depth: Option<u32>,
    /// This agent's bound messaging channels (Telegram, email, ...). A given
    /// `ChannelKind` normally appears at most once; enforcing that is the
    /// caller's job (see [`AgentProfile::channel_of_kind`]). Absent in
    /// profiles predating this field, which deserialise as an empty vec.
    ///
    /// Profiles saved before this field existed instead carried a single
    /// `telegram: Option<TelegramConfig>` field. That legacy shape is still
    /// accepted on input — see [`AgentProfileWire`] — and folded into a
    /// single-element `channels` vec, but is never re-emitted on output.
    #[serde(default)]
    pub channels: Vec<ChannelBinding>,
}

/// Which external channel a [`ChannelBinding`] connects an agent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Telegram,
    Discord,
    Email,
    Slack,
    WhatsApp,
    Webhook,
}

impl ChannelKind {
    /// Stable lowercase identifier, matching the serde wire form. Used
    /// anywhere a `ChannelKind` needs to appear outside serde, e.g. as part
    /// of a deterministic `ChannelBinding::binding_id`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ChannelKind::Telegram => "telegram",
            ChannelKind::Discord => "discord",
            ChannelKind::Email => "email",
            ChannelKind::Slack => "slack",
            ChannelKind::WhatsApp => "whatsapp",
            ChannelKind::Webhook => "webhook",
        }
    }
}

/// One messaging channel bound to an agent — e.g. a Telegram bot, or an
/// inbox polled over IMAP. Generalizes the single-purpose `TelegramConfig`
/// that used to live directly on `AgentProfile` so future channel kinds
/// don't each need their own top-level `AgentProfile` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelBinding {
    /// Stable id for this binding, scoped to the owning agent's profile
    /// (this struct itself lives on one `AgentProfile`) — but the *value* is
    /// not unique across agents: the migration from the legacy `telegram`
    /// field always assigns the deterministic id `"telegram"` so migrating
    /// the same profile more than once is idempotent, which means every
    /// agent with a Telegram binding gets the identical string `"telegram"`.
    /// Any store keyed on `binding_id` alone (e.g.
    /// `ao_persistence::conversation_registry_store::ConversationRegistryStore`)
    /// must also key on `agent_id`, or two different agents sharing a
    /// channel kind collide on the same row/file — see that store's module
    /// doc.
    pub binding_id: String,
    pub kind: ChannelKind,
    pub enabled: bool,
    /// The single thread all inbound/outbound traffic for this binding
    /// flows through. Provisioned once, when the binding is enabled
    /// (server-owned: a client-supplied value is never trusted over the
    /// already-stored one). `None` until enabling has provisioned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_thread_id: Option<String>,
    /// Sender identities linked to this agent through the binding's pairing
    /// or allow-list flow (e.g. Telegram chat ids, stringified). Empty means
    /// no sender has been linked yet.
    ///
    /// DEPRECATED: authoritative home is LinkedSenderStore; retained only
    /// for one-time backfill. This field lived inline on the whole
    /// `AgentProfile` document, so an out-of-band writer (Telegram pairing)
    /// and a general profile save could each round-trip the whole document
    /// and clobber the other's change to this one field. Every enforcement
    /// read and write now goes through `ao_persistence::linked_sender_store`
    /// instead; this field is only ever consulted once, to seed that store
    /// the first time a pre-existing binding is read, so migrating agents
    /// never lose access they already had.
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    /// A pairing code awaiting a sender to claim it. Cleared once a sender
    /// links successfully or once the code expires. `None` when no pairing
    /// is currently in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_pairing_code: Option<PairingCode>,
    pub kind_config: ChannelKindConfig,
}

impl ChannelBinding {
    /// Builds the deterministic Telegram binding used to migrate a
    /// pre-`channels` profile's bare `telegram` field — see
    /// [`AgentProfileWire`]. Always uses the stable id `"telegram"`.
    pub fn from_legacy_telegram(telegram: TelegramConfig) -> Self {
        Self {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: telegram.enabled,
            bridge_thread_id: telegram.bridge_thread_id,
            allowed_senders: telegram
                .allowed_chat_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
            pending_pairing_code: telegram.pending_pairing_code,
            kind_config: ChannelKindConfig::Telegram {
                bot_username: telegram.bot_username,
                thread_mode: telegram.thread_mode,
            },
        }
    }
}

/// Kind-specific configuration nested inside a [`ChannelBinding`]. Fields
/// shared by every channel kind (enablement, bridge thread, linked senders,
/// pairing code) live directly on `ChannelBinding` instead; this enum only
/// holds what's specific to one kind of channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ChannelKindConfig {
    Telegram {
        /// Cached bot display name (e.g. `"@axew_research_bot"`), captured
        /// from a `getMe` call once the bridge first connects. `None` until
        /// then.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bot_username: Option<String>,
        #[serde(default)]
        thread_mode: TelegramThreadMode,
    },
    Email {
        address: String,
        #[serde(default)]
        imap_host: String,
        #[serde(default)]
        imap_port: u16,
        #[serde(default)]
        smtp_host: String,
        #[serde(default)]
        smtp_port: u16,
        #[serde(default)]
        poll_secs: u32,
        /// Fails CLOSED on a malformed/legacy stored profile missing this
        /// field: defaults to `true` (reject unauthenticated senders) rather
        /// than `false`, so a profile written before this field existed (or
        /// corrupted to drop it) doesn't silently start accepting spoofable
        /// mail.
        #[serde(default = "default_true")]
        require_auth_results: bool,
    },
    Discord {
        /// Discord user IDs (or usernames) permitted to trigger the agent.
        /// OR-combined with `allowed_roles` — a sender needs to match either
        /// list. Empty means no user is individually allow-listed yet, which
        /// combines with an empty `allowed_roles` to fail closed and reject
        /// everyone, matching Email's `allowed_senders` semantics.
        #[serde(default)]
        allowed_users: Vec<String>,
        /// Guild role IDs permitted to trigger the agent. OR-combined with
        /// `allowed_users`.
        #[serde(default)]
        allowed_roles: Vec<String>,
        /// Optional channel-ID allow-list. Empty means every channel the bot
        /// can see is eligible, subject to the user/role checks above.
        #[serde(default)]
        allowed_channels: Vec<String>,
        /// Guild whose roles authorize direct messages, since a DM has no
        /// guild of its own to resolve roles against. `None` disables
        /// role-based auth for DMs (only `allowed_users` applies there).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dm_role_auth_guild: Option<String>,
        /// Whether the bot only responds to a message in a guild channel
        /// that explicitly @-mentions it. `false` reverts to responding to
        /// every authorized message in every guild channel — an escape
        /// hatch for a private single-user server where requiring a mention
        /// is just friction. Never affects DMs or a thread the bot itself
        /// created, both of which always respond regardless of this flag.
        #[serde(default = "default_true")]
        require_mention: bool,
        /// How long a thread the bot was mentioned in keeps responding
        /// without a fresh mention — see [`ThreadFollowMode`]. Only a
        /// thread can ever warm this way; an ordinary (non-thread) guild
        /// channel always stays mention-only regardless of this setting.
        #[serde(default)]
        thread_follow: ThreadFollowMode,
        /// [`ThreadFollowMode::StickyDecay`]'s idle-timeout knob, in
        /// minutes since the thread's last mention. Ignored by the other
        /// two modes.
        #[serde(default = "default_thread_idle_timeout_minutes")]
        thread_idle_timeout_minutes: u32,
        /// [`ThreadFollowMode::StickyDecay`]'s consecutive-unmentioned-message
        /// budget before the thread decays back to mention-only. Ignored by
        /// the other two modes.
        #[serde(default = "default_thread_message_budget")]
        thread_message_budget: u32,
        /// How many prior messages to fetch as context the first time the
        /// bot responds in a conversation (a thread's message window, or a
        /// reply chain outside a thread). `0` disables history backfill
        /// entirely.
        #[serde(default = "default_backfill_limit")]
        backfill_limit: u32,
    },
    Slack {
        /// Slack channel IDs (`C…`/`D…`/`G…`) permitted to trigger the
        /// agent. Empty means no channel is allow-listed yet, which fails
        /// CLOSED and rejects every conversation until at least one channel
        /// is added — the same reject-all-when-empty semantics as
        /// Telegram's `allowed_chat_ids` and Discord's `allowed_channels`.
        /// Enforcement lands with the transport; this field only carries
        /// the intent today.
        #[serde(default)]
        allowed_channels: Vec<String>,
        /// Slack user IDs (`U…`) permitted to trigger the agent. Same
        /// reject-all-when-empty semantics as `allowed_channels` above, and
        /// same enforcement note.
        #[serde(default)]
        allowed_users: Vec<String>,
        /// Reference to this binding's workspace-level
        /// `ao_protocol::slack_connection::SlackConnection` record.
        /// Holds neither the bot user id, the
        /// team id, nor either token directly: those live on the connection
        /// record (identity) and in `ChannelSecretStore` (the two tokens),
        /// looked up by this id. `None` until a successful Test Connection
        /// has provisioned the connection record. One binding points at one
        /// connection today — Slack ships one app per agent — but the
        /// indirection is what makes a future N-bindings-one-connection
        /// world a lookup change, not a schema migration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection_id: Option<String>,
        #[serde(default)]
        conversation_mode: SlackConversationMode,
    },
}

/// How a Slack conversation (a DM, a channel `@mention` thread, or a reply
/// inside one) maps onto a Launchpad bridge thread. Locked 1:1-per-conversation
/// — a correctness requirement, not a preference, so there is only one
/// variant today. Kept as an enum rather than folded into code so
/// a future conversation-shaping mode (e.g. one dedicated thread across an
/// entire binding, mirroring `TelegramThreadMode::Dedicated`) is a new
/// variant instead of a breaking schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SlackConversationMode {
    /// One Launchpad thread per Slack conversation: one per DM, one per
    /// Slack thread.
    #[default]
    PerConversation,
}

/// Serde default helper for fields that must fail CLOSED (not open) when
/// absent from a stored value — see `ChannelKindConfig::Email::require_auth_results`.
fn default_true() -> bool {
    true
}

fn default_thread_idle_timeout_minutes() -> u32 {
    15
}

fn default_thread_message_budget() -> u32 {
    10
}

fn default_backfill_limit() -> u32 {
    20
}

/// How long a thread the bot was mentioned in keeps responding without a
/// fresh mention. Mirrors `ao_engine::channels::discord::engagement`'s
/// runtime semantics one-for-one — see that module for exactly what each
/// variant does to the cold/warm state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThreadFollowMode {
    /// Answer the mentioned message, then immediately return to
    /// mention-only — the thread never stays warm.
    OneShot,
    /// Stay warm for a while after a mention, decaying back to
    /// mention-only after `thread_idle_timeout_minutes` of no further
    /// mention or `thread_message_budget` unmentioned messages, whichever
    /// comes first.
    #[default]
    StickyDecay,
    /// Stay warm forever once mentioned — never decays.
    Always,
}

impl AgentProfile {
    /// First channel binding of the given kind, if any.
    pub fn channel_of_kind(&self, kind: ChannelKind) -> Option<&ChannelBinding> {
        self.channels.iter().find(|binding| binding.kind == kind)
    }

    /// Mutable form of [`Self::channel_of_kind`].
    pub fn channel_of_kind_mut(&mut self, kind: ChannelKind) -> Option<&mut ChannelBinding> {
        self.channels
            .iter_mut()
            .find(|binding| binding.kind == kind)
    }

    /// Convenience wrapper over `channel_of_kind(ChannelKind::Telegram)`.
    pub fn telegram_binding(&self) -> Option<&ChannelBinding> {
        self.channel_of_kind(ChannelKind::Telegram)
    }

    /// Mutable form of [`Self::telegram_binding`].
    pub fn telegram_binding_mut(&mut self) -> Option<&mut ChannelBinding> {
        self.channel_of_kind_mut(ChannelKind::Telegram)
    }

    /// Compatibility shim: reconstructs the pre-migration `TelegramConfig` shape
    /// from this profile's Telegram channel binding, for runtime code not
    /// yet ported onto `channels`/`ChannelBinding` directly. Remove once
    /// bridge.rs, outbound.rs, and the telegram/agents routes read
    /// `ChannelBinding` natively.
    pub fn telegram_config_view(&self) -> Option<TelegramConfig> {
        let binding = self.telegram_binding()?;
        let ChannelKindConfig::Telegram {
            bot_username,
            thread_mode,
        } = &binding.kind_config
        else {
            return None;
        };
        Some(TelegramConfig {
            enabled: binding.enabled,
            bot_username: bot_username.clone(),
            thread_mode: *thread_mode,
            bridge_thread_id: binding.bridge_thread_id.clone(),
            allowed_chat_ids: binding
                .allowed_senders
                .iter()
                .filter_map(|sender| sender.parse().ok())
                .collect(),
            pending_pairing_code: binding.pending_pairing_code.clone(),
        })
    }

    /// Compatibility shim: write side of [`Self::telegram_config_view`] — upserts
    /// the Telegram channel binding from an owned `TelegramConfig`, or
    /// removes it on `None`. Same removal note as the getter.
    pub fn set_telegram_config(&mut self, config: Option<TelegramConfig>) {
        self.channels.retain(|binding| binding.kind != ChannelKind::Telegram);
        if let Some(config) = config {
            self.channels.push(ChannelBinding::from_legacy_telegram(config));
        }
    }
}

/// Wire-format shadow of [`AgentProfile`], used only to deserialize it (via
/// `#[serde(from = "AgentProfileWire")]` on the real struct). Exists solely
/// to accept the legacy pre-`channels` shape: a profile saved before this
/// migration carries a bare `telegram: Option<TelegramConfig>` field instead
/// of `channels`. When `channels` is absent or empty and `telegram` is
/// present, it's folded into a single-element `channels` vec. A profile that
/// already has a non-empty `channels` list round-trips untouched — `telegram`
/// is ignored in that case, and never re-emitted on serialize either way
/// (`AgentProfile`'s own `Serialize` impl only knows about `channels`).
#[derive(Deserialize)]
struct AgentProfileWire {
    id: AgentId,
    name: String,
    description: String,
    #[serde(default)]
    emoji: Option<String>,
    provider: ProviderConfig,
    model: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    system_prompt: Option<String>,
    tools: Option<ToolsConfig>,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
    #[serde(default = "default_max_instances")]
    max_instances: u32,
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: u64,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    home_dir: Option<String>,
    #[serde(default = "default_serialize")]
    serialize: bool,
    #[serde(default)]
    workflows: Option<WorkflowBinding>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    runner_mode: AgentRunnerMode,
    #[serde(default)]
    enabled_plugins: std::collections::HashMap<String, PluginEnablement>,
    #[serde(default)]
    enabled_launchpad_global_skills: Option<Vec<String>>,
    #[serde(default)]
    enabled_launchpad_project_skills: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    owning_team_id: Option<String>,
    #[serde(default)]
    native_provider: Option<NativeProvider>,
    #[serde(default)]
    thinking: Option<ThinkingConfig>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    max_context_tokens: Option<u32>,
    #[serde(default)]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    delegates_to: Vec<DelegateTarget>,
    #[serde(default)]
    persona: Option<String>,
    #[serde(default)]
    special_instructions: Option<String>,
    #[serde(default)]
    legacy_system_prompt: Option<String>,
    #[serde(default)]
    max_delegation_depth: Option<u32>,
    /// New-shape channel bindings. Present (possibly empty) on any profile
    /// saved after this migration landed.
    #[serde(default)]
    channels: Vec<ChannelBinding>,
    /// Legacy pre-migration shape — only ever populated on profiles saved
    /// before `channels` existed. Folded into `channels` in the `From` impl
    /// below when `channels` came back empty.
    #[serde(default)]
    telegram: Option<TelegramConfig>,
}

impl From<AgentProfileWire> for AgentProfile {
    fn from(wire: AgentProfileWire) -> Self {
        let channels = if wire.channels.is_empty() {
            match wire.telegram {
                Some(telegram) => vec![ChannelBinding::from_legacy_telegram(telegram)],
                None => vec![],
            }
        } else {
            wire.channels
        };
        AgentProfile {
            id: wire.id,
            name: wire.name,
            description: wire.description,
            emoji: wire.emoji,
            provider: wire.provider,
            model: wire.model,
            skills: wire.skills,
            system_prompt: wire.system_prompt,
            tools: wire.tools,
            env: wire.env,
            max_instances: wire.max_instances,
            timeout_seconds: wire.timeout_seconds.clamp(1, MAX_TIMEOUT_SECONDS),
            max_turns: wire.max_turns,
            working_dir: wire.working_dir,
            home_dir: wire.home_dir,
            serialize: wire.serialize,
            workflows: wire.workflows,
            template: wire.template,
            runner_mode: wire.runner_mode,
            enabled_plugins: wire.enabled_plugins,
            enabled_launchpad_global_skills: wire.enabled_launchpad_global_skills,
            enabled_launchpad_project_skills: wire.enabled_launchpad_project_skills,
            owning_team_id: wire.owning_team_id,
            native_provider: wire.native_provider,
            thinking: wire.thinking,
            max_output_tokens: wire.max_output_tokens,
            max_context_tokens: wire.max_context_tokens,
            reasoning_effort: wire.reasoning_effort,
            delegates_to: wire.delegates_to,
            persona: wire.persona,
            special_instructions: wire.special_instructions,
            legacy_system_prompt: wire.legacy_system_prompt,
            max_delegation_depth: wire.max_delegation_depth,
            channels,
        }
    }
}

/// Per-agent enablement state for a single plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginEnablement {
    pub enabled: bool,
    /// `None` means every skill the plugin ships is enabled.
    /// `Some(list)` restricts to the listed bare skill names (e.g. `"tdd"`, not `"superpowers/tdd"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_skills: Option<Vec<String>>,
}

impl AgentProfile {
    /// Returns true if the agent's provider has file capabilities enabled.
    pub fn file_capabilities_supported(&self) -> bool {
        match &self.provider {
            ProviderConfig::Cli(cli) => cli
                .file_capabilities
                .as_ref()
                .map(|fc| fc.supported)
                .unwrap_or(false),
        }
    }

    pub fn is_plugin_enabled(&self, plugin_name: &str) -> bool {
        self.enabled_plugins
            .get(plugin_name)
            .is_some_and(|e| e.enabled)
    }

    pub fn is_skill_enabled(&self, plugin_name: &str, skill_name: &str) -> bool {
        let Some(entry) = self.enabled_plugins.get(plugin_name) else {
            return false;
        };
        if !entry.enabled {
            return false;
        }
        match &entry.enabled_skills {
            None => true,
            Some(list) => list.iter().any(|s| s == skill_name),
        }
    }

    pub fn set_plugin_enabled(&mut self, plugin_name: &str, enabled: bool) {
        let entry = self
            .enabled_plugins
            .entry(plugin_name.to_string())
            .or_insert(PluginEnablement {
                enabled,
                enabled_skills: None,
            });
        entry.enabled = enabled;
    }

    pub fn set_skill_subset(&mut self, plugin_name: &str, subset: Option<Vec<String>>) {
        let entry = self
            .enabled_plugins
            .entry(plugin_name.to_string())
            .or_insert(PluginEnablement {
                enabled: true,
                enabled_skills: None,
            });
        entry.enabled_skills = subset;
    }
}

/// Canonicalizes a focus path into the deterministic key used by
/// `AgentProfile::enabled_launchpad_project_skills` to scope per-project
/// convention-folder skill enablement.
///
/// When `focus_path` exists on disk, resolves it via [`std::fs::canonicalize`]
/// (following symlinks) so two different-looking paths to the same directory
/// map to the same key. When it does not exist — e.g. a project that has been
/// moved or unmounted since a profile last recorded it — falls back to purely
/// lexical normalization (trailing `/`/`\\` stripped) so the function never
/// fails and always returns a stable value for a given input.
pub fn canonical_project_key(focus_path: &str) -> String {
    if let Ok(canonical) = std::fs::canonicalize(focus_path) {
        return canonical.to_string_lossy().into_owned();
    }

    let mut key = focus_path.to_string();
    while key.len() > 1 && (key.ends_with('/') || key.ends_with('\\')) {
        key.pop();
    }
    key
}

fn default_max_instances() -> u32 {
    1
}

fn default_timeout_seconds() -> u64 {
    300
}

/// Upper bound on [`AgentProfile::timeout_seconds`]. Enforced in
/// `From<AgentProfileWire>` — the single choke point every profile passes
/// through on load (disk persistence and the `POST`/`PUT /agents` JSON
/// bodies alike, since `AgentProfile` derives `Deserialize` via
/// `#[serde(from = "AgentProfileWire")]`). One hour comfortably covers a
/// long agentic turn; anything past it is far more likely a units slip
/// (e.g. a caller pasting a millisecond value into this seconds field)
/// than an intentional setting, and an unclamped value here is multiplied
/// into a background CLI process's hard wall-clock deadline
/// (`agent_runner::cli`'s `bg_timeout_ms = agent.timeout_seconds * 1000`),
/// so a units slip can otherwise leave an OS process alive for days.
const MAX_TIMEOUT_SECONDS: u64 = 3600;

fn default_serialize() -> bool {
    true
}

/// Fallback cap on model-completion turns for a native (in-process API)
/// agent run when [`AgentProfile::max_turns`] is unset. Guards against a
/// run stuck in a tool-error retry loop calling the model unboundedly — in
/// native mode those calls are billed straight against the end user's own
/// provider API key, unlike the CLI runner's spawned subprocess, which sits
/// under a supervisor watchdog regardless of this setting.
///
/// 50 is the chosen default, not a placeholder — it's only ever consulted
/// when a profile doesn't specify its own `max_turns`; an explicit per-agent
/// value always takes precedence over this constant.
pub const DEFAULT_MAX_TURNS: u32 = 50;

/// Selects which native (in-process API) provider client to instantiate.
///
/// Used by `DefaultProviderFactory` when `runner_mode = Api`. Agents that
/// run through the CLI path (`runner_mode = Cli`) ignore this field.
/// Absent (None) defaults to Anthropic for backwards compatibility.
///
/// `OpenRouter` speaks the same OpenAI-compatible Chat Completions API as
/// `Openai`, so it is routed through the same `OpenAIClient` transport with
/// a different `providers.toml` section (its own base URL and default
/// model) rather than a dedicated provider crate — see
/// `DefaultProviderFactory::build`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeProvider {
    Anthropic,
    Openai,
    OpenRouter,
}

/// Controls whether the model is asked to engage its dedicated reasoning
/// channel before producing the final response. The protocol layer expresses
/// this as a provider-neutral enum; each provider's spawn layer maps these
/// values to its own concrete API/CLI surface (e.g. Anthropic Claude CLI
/// translates `Adaptive` → `--thinking adaptive`).
///
/// `Disabled` is the only way to *opt out* — the absence of `ThinkingConfig`
/// on a profile means "use the provider's default", which for the Claude CLI
/// is itself adaptive thinking but with `display = "omitted"` (i.e. no visible
/// reasoning text on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    /// Let the provider decide how much reasoning to engage on a per-prompt
    /// basis. Reasoning-heavy turns get more thinking tokens; trivial turns
    /// short-circuit through the channel.
    #[default]
    Adaptive,
    /// Skip the reasoning channel entirely. Maps to the provider's "no
    /// thinking" mode where available; the spawn layer for providers that
    /// can't disable thinking simply omits the flag set.
    Disabled,
}

/// How much of the model's reasoning to surface to the client. Mirrors the
/// canonical Anthropic API field (`thinking.display`) verbatim — provider
/// neutrality is preserved by the spawn-layer mapping, not by inventing a
/// novel vocabulary here.
///
/// * `Summarized` — provider returns a digested summary of the reasoning.
///   Right tradeoff for chat UIs: cheap enough to render mid-stream, hides
///   the raw chain-of-thought verbosity.
/// * `Raw` — full reasoning text, character-for-character. Useful for
///   debugging or transparency-heavy UIs; expensive to render for long
///   reasoning chains.
/// * `Omitted` — no reasoning text on the wire; only the cryptographic
///   signature proving thinking occurred. This is the Claude CLI's default
///   behavior and the reason a "Thinking…" indicator alone is sometimes the
///   only signal an in-progress turn provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    #[default]
    Summarized,
    Raw,
    Omitted,
}

/// Per-agent reasoning channel configuration. Lives on `AgentProfile` so the
/// same profile can drive a CLI runner today and an API runner tomorrow
/// without the caller needing to translate provider-specific flag names.
///
/// `budget_tokens` is an optional hard cap on the number of tokens the
/// provider is allowed to spend in the reasoning channel for a single turn.
/// When `None`, the provider's own default budget applies (typically tied to
/// model + mode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(default)]
    pub mode: ThinkingMode,
    #[serde(default)]
    pub display: ThinkingDisplay,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self {
            mode: ThinkingMode::default(),
            display: ThinkingDisplay::default(),
            budget_tokens: None,
        }
    }
}

/// A small ordinal set of reasoning-effort levels for the native (API)
/// provider path. Deliberately not a raw token count: an operator picks
/// "how hard should this agent think", not a specific budget number, and
/// the request builder for each provider maps the level onto whatever wire
/// shape that provider actually accepts (a token budget for Anthropic's
/// extended thinking, the native `reasoning_effort` string for
/// OpenAI-compatible chat completions).
///
/// This is a separate knob from [`ThinkingConfig`] above, not a replacement
/// for it. `ThinkingConfig` is the older, per-turn mechanism the CLI runner
/// (`agent_runner::cli`) maps onto `--thinking`/`--thinking-display` flags,
/// and which the Anthropic API path also already honors verbatim via
/// `CompletionRequest.thinking` with no persisted-config or provider-default
/// fallback. `ReasoningEffort` is resolved with the same per-agent ??
/// persisted-config ?? provider-default precedence as [`AgentProfile::model`]
/// (see `ao_engine_tools_runner::provider::resolve_reasoning_effort`) and is
/// baked into the provider client at construction time via `with_reasoning_effort`,
/// exactly like model resolution. When both are present on a request, the
/// explicit per-turn `ThinkingConfig` wins — it is strictly more specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// The wire/TOML string for this level — matches the `#[serde(rename_all
    /// = "snake_case")]` encoding exactly, so callers that need the string
    /// outside of serde (writing `providers.toml` via `toml_edit`, building
    /// an OpenAI-compatible `reasoning_effort` request field) get a value
    /// that round-trips through `serde_json`/`toml` without drift.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }
}

/// Non-secret Telegram bridge configuration for a single agent. The bot
/// token is deliberately excluded from this struct — it must never touch
/// the plaintext profile YAML — and instead lives in `TelegramTokenStore`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelegramConfig {
    pub enabled: bool,
    /// Cached bot display name (e.g. `"@axew_research_bot"`), captured from
    /// a `getMe` call once the bridge first connects. `None` until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,
    #[serde(default)]
    pub thread_mode: TelegramThreadMode,
    /// The single thread all inbound/outbound channel traffic for this
    /// binding flows through. Provisioned once, when the binding is enabled
    /// (server-owned: a client-supplied value is never trusted over the
    /// already-stored one). `None` until enabling has provisioned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_thread_id: Option<String>,
    /// Chat IDs linked to this agent through the pairing flow. Empty means
    /// no chat has been linked yet.
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
    /// A pairing code awaiting a chat to claim it. Cleared once a chat links
    /// successfully or once the code expires. `None` when no pairing is
    /// currently in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_pairing_code: Option<PairingCode>,
}

/// Characters usable in a generated `PairingCode`. Excludes glyphs that are
/// easily confused when read aloud or typed by hand (`0`/`O`, `1`/`I`/`L`).
pub const PAIRING_CODE_ALPHABET: &str = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Number of characters in a generated pairing code.
pub const PAIRING_CODE_LENGTH: usize = 6;

/// How long a generated pairing code remains valid, in seconds.
pub const PAIRING_CODE_TTL_SECONDS: i64 = 600;

/// A short-lived, human-typeable code used to link a Telegram chat to an
/// agent through the pairing flow. Callers own the wall clock: this type
/// never reads it directly, so it stays deterministic and testable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairingCode {
    pub code: String,
    pub expires_at_unix: i64,
}

impl PairingCode {
    /// Generates a new pairing code expiring `PAIRING_CODE_TTL_SECONDS`
    /// (10 minutes) after `now_unix`.
    pub fn generate(now_unix: i64) -> Self {
        let alphabet = PAIRING_CODE_ALPHABET.as_bytes();
        let random_bytes = uuid::Uuid::new_v4().into_bytes();
        let code: String = random_bytes[..PAIRING_CODE_LENGTH]
            .iter()
            .map(|byte| alphabet[(*byte as usize) % alphabet.len()] as char)
            .collect();
        Self {
            code,
            expires_at_unix: now_unix + PAIRING_CODE_TTL_SECONDS,
        }
    }

    /// Whether this code is no longer usable at `now_unix`. A code is
    /// considered expired at the exact expiry instant, not just after it.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        now_unix >= self.expires_at_unix
    }
}

/// How an incoming Telegram chat maps to an agent's conversation threads.
/// Only one strategy exists today; the enum leaves room to add alternatives
/// (e.g. always routing to the agent's main thread) in a later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TelegramThreadMode {
    /// All correspondence for this binding flows through one dedicated
    /// thread (`TelegramConfig::bridge_thread_id`), not a thread per chat.
    #[default]
    Dedicated,
}

/// How an agent reaches its model. One variant today, so destructuring is
/// irrefutable — `let ProviderConfig::Cli(cli) = &profile.provider;` with no
/// `else` arm is the expected form at call sites, not an oversight.
///
/// It stays an enum because the internal `type` tag is part of the persisted
/// format: every agent profile on disk serializes its provider as
/// `type: Cli` followed by the CLI fields (see `ao-persistence`'s profile
/// store, which writes these as YAML). Collapsing this to a plain struct
/// would drop that key and change the shape of every stored profile; keeping
/// the enum means a second provider kind can be added without touching
/// profiles already written.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ProviderConfig {
    Cli(CliProviderConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CliProviderConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub normalizer: Option<String>,
    #[serde(default = "default_output_format")]
    pub output_format: OutputFormat,
    #[serde(default = "default_input_mode")]
    pub input_mode: InputMode,
    pub model_arg: Option<String>,
    #[serde(default)]
    pub model_aliases: std::collections::HashMap<String, String>,
    pub system_prompt_arg: Option<String>,
    pub session_arg: Option<String>,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub session_id_fields: Vec<String>,
    #[serde(default)]
    pub clear_env: bool,
    #[serde(default = "default_no_output_timeout_ms")]
    pub no_output_timeout_ms: u64,
    #[serde(default)]
    pub file_capabilities: Option<FileCapability>,
}

fn default_output_format() -> OutputFormat {
    OutputFormat::Text
}

fn default_input_mode() -> InputMode {
    InputMode::Arg
}

fn default_no_output_timeout_ms() -> u64 {
    30000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OutputFormat {
    Json,
    Jsonl,
    Text,
    StreamJson,
    StreamJsonl,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum InputMode {
    Arg,
    Stdin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolsConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub require_approval: Vec<String>,
}

/// Controls which workflows an agent can access.
///
/// Serializes as:
/// - `All` → the string `"all"`
/// - `List(vec)` → a YAML list of strings
/// - `None` → field is omitted (via Option<WorkflowBinding> + skip_serializing_if)
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowBinding {
    All,
    List(Vec<String>),
    None,
}

impl Serialize for WorkflowBinding {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            WorkflowBinding::All => serializer.serialize_str("all"),
            WorkflowBinding::List(ids) => ids.serialize(serializer),
            WorkflowBinding::None => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for WorkflowBinding {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(s) if s == "all" => Ok(WorkflowBinding::All),
            serde_json::Value::Array(arr) => {
                let ids: Vec<String> = arr
                    .into_iter()
                    .map(|v| match v {
                        serde_json::Value::String(s) => Ok(s),
                        _ => Err(serde::de::Error::custom("expected string in workflow list")),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(WorkflowBinding::List(ids))
            }
            serde_json::Value::Null => Ok(WorkflowBinding::None),
            _ => Err(serde::de::Error::custom(
                "expected 'all', a list of strings, or null for workflows",
            )),
        }
    }
}

#[cfg(test)]
mod plugin_enablement_tests {
    use super::*;
    use std::collections::HashMap;

    fn make_profile() -> AgentProfile {
        AgentProfile {
            id: "a".into(),
            name: "a".into(),
            description: "".into(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "claude".into(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: AgentRunnerMode::Cli,
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            max_turns: None,
        }
    }

    #[test]
    fn default_profile_has_empty_enabled_plugins_map() {
        let p = make_profile();
        assert!(p.enabled_plugins.is_empty());
        assert!(!p.is_plugin_enabled("anything"));
        assert!(!p.is_skill_enabled("anything", "tdd"));
    }

    #[test]
    fn set_plugin_enabled_inserts_and_toggles() {
        let mut p = make_profile();
        p.set_plugin_enabled("superpowers", true);
        assert!(p.is_plugin_enabled("superpowers"));
        // subset unset -> every skill counts as enabled
        assert!(p.is_skill_enabled("superpowers", "tdd"));

        p.set_plugin_enabled("superpowers", false);
        assert!(!p.is_plugin_enabled("superpowers"));
        // disabled plugin -> no skills enabled
        assert!(!p.is_skill_enabled("superpowers", "tdd"));
    }

    #[test]
    fn set_skill_subset_restricts_and_reverts() {
        let mut p = make_profile();
        p.set_plugin_enabled("superpowers", true);
        p.set_skill_subset("superpowers", Some(vec!["tdd".into(), "git".into()]));

        assert!(p.is_skill_enabled("superpowers", "tdd"));
        assert!(p.is_skill_enabled("superpowers", "git"));
        assert!(!p.is_skill_enabled("superpowers", "debugger"));

        // Reverting to None re-enables every skill.
        p.set_skill_subset("superpowers", None);
        assert!(p.is_skill_enabled("superpowers", "tdd"));
        assert!(p.is_skill_enabled("superpowers", "debugger"));
    }

    #[test]
    fn set_skill_subset_without_prior_plugin_entry_enables_it() {
        let mut p = make_profile();
        // The UI may call set_skill_subset first when the user ticks a specific
        // skill on an otherwise-untouched plugin row — auto-enable the plugin.
        p.set_skill_subset("superpowers", Some(vec!["tdd".into()]));
        assert!(p.is_plugin_enabled("superpowers"));
        assert!(p.is_skill_enabled("superpowers", "tdd"));
        assert!(!p.is_skill_enabled("superpowers", "git"));
    }

    #[test]
    fn toggling_enabled_preserves_existing_subset() {
        let mut p = make_profile();
        p.set_plugin_enabled("superpowers", true);
        p.set_skill_subset("superpowers", Some(vec!["tdd".into()]));

        p.set_plugin_enabled("superpowers", false);
        p.set_plugin_enabled("superpowers", true);

        // Subset survived the off/on cycle.
        assert!(p.is_skill_enabled("superpowers", "tdd"));
        assert!(!p.is_skill_enabled("superpowers", "git"));
    }

    #[test]
    fn disabled_plugin_with_subset_reports_no_skill_enabled() {
        let mut p = make_profile();
        p.set_plugin_enabled("superpowers", true);
        p.set_skill_subset("superpowers", Some(vec!["tdd".into()]));
        p.set_plugin_enabled("superpowers", false);

        // Plugin-level toggle is authoritative — subset doesn't leak through.
        assert!(!p.is_skill_enabled("superpowers", "tdd"));
    }

    #[test]
    fn legacy_yaml_without_enabled_plugins_deserializes_as_empty() {
        // Migration-safe: an older agent profile YAML lacking the field loads
        // cleanly and the helper methods return sensible defaults.
        let yaml = r#"
id: legacy
name: Legacy
description: An agent from before plugins existed.
provider:
  type: Cli
  command: claude
  args: []
model: null
system_prompt: null
tools: null
max_instances: 1
timeout_seconds: 300
serialize: true
"#;
        let profile: AgentProfile =
            serde_yaml::from_str(yaml).expect("legacy profile should deserialize");
        assert!(profile.enabled_plugins.is_empty());
        assert!(!profile.is_plugin_enabled("anything"));
    }

    #[test]
    fn yaml_round_trip_with_populated_enabled_plugins() {
        let mut p = make_profile();
        p.set_plugin_enabled("superpowers", true);
        p.set_skill_subset("superpowers", Some(vec!["tdd".into()]));
        p.set_plugin_enabled("karpathy", true);

        let yaml = serde_yaml::to_string(&p).expect("serialize");
        let decoded: AgentProfile = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(p.enabled_plugins, decoded.enabled_plugins);
    }

    #[test]
    fn empty_enabled_plugins_is_omitted_from_serialized_yaml() {
        let p = make_profile();
        let yaml = serde_yaml::to_string(&p).expect("serialize");
        assert!(
            !yaml.contains("enabled_plugins"),
            "empty enabled_plugins should be skipped: {yaml}"
        );
    }

    #[test]
    fn legacy_profile_without_owning_team_id_deserializes_as_none() {
        // Existing on-disk agent profile JSON predates the owning_team_id flag —
        // it must continue to deserialize and resolve to None.
        let json = r#"{
            "id": "legacy",
            "name": "Legacy",
            "description": "An agent from before team-owned coordinators existed.",
            "provider": { "type": "Cli", "command": "claude", "args": [] },
            "model": null,
            "system_prompt": null,
            "tools": null,
            "max_instances": 1,
            "timeout_seconds": 300,
            "serialize": true
        }"#;
        let profile: AgentProfile =
            serde_json::from_str(json).expect("legacy profile should deserialize");
        assert!(profile.owning_team_id.is_none());
    }

    #[test]
    fn timeout_seconds_absurd_value_is_clamped_on_deserialize() {
        // A units slip (e.g. a caller pasting a millisecond value into this
        // seconds field) must not survive deserialization uncapped — see
        // MAX_TIMEOUT_SECONDS's doc comment for why an unclamped value here
        // was reaching a background CLI process's hard wall-clock deadline.
        let json = r#"{
            "id": "units-slip",
            "name": "Units Slip",
            "description": "",
            "provider": { "type": "Cli", "command": "claude", "args": [] },
            "model": null,
            "system_prompt": null,
            "tools": null,
            "max_instances": 1,
            "timeout_seconds": 300000,
            "serialize": true
        }"#;
        let profile: AgentProfile = serde_json::from_str(json).expect("profile should deserialize");
        assert_eq!(profile.timeout_seconds, MAX_TIMEOUT_SECONDS);
    }

    #[test]
    fn timeout_seconds_zero_is_clamped_up_to_one() {
        let json = r#"{
            "id": "zero-timeout",
            "name": "Zero Timeout",
            "description": "",
            "provider": { "type": "Cli", "command": "claude", "args": [] },
            "model": null,
            "system_prompt": null,
            "tools": null,
            "max_instances": 1,
            "timeout_seconds": 0,
            "serialize": true
        }"#;
        let profile: AgentProfile = serde_json::from_str(json).expect("profile should deserialize");
        assert_eq!(profile.timeout_seconds, 1);
    }

    #[test]
    fn timeout_seconds_within_range_passes_through_unchanged() {
        let json = r#"{
            "id": "normal-timeout",
            "name": "Normal Timeout",
            "description": "",
            "provider": { "type": "Cli", "command": "claude", "args": [] },
            "model": null,
            "system_prompt": null,
            "tools": null,
            "max_instances": 1,
            "timeout_seconds": 1800,
            "serialize": true
        }"#;
        let profile: AgentProfile = serde_json::from_str(json).expect("profile should deserialize");
        assert_eq!(profile.timeout_seconds, 1800);
    }

    #[test]
    fn legacy_profile_yaml_loads_with_cli_runner_mode() {
        let yaml = include_str!("fixtures/legacy_agent_profile.yaml");
        let profile: AgentProfile = serde_yaml::from_str(yaml).expect("legacy profile should deserialize");
        assert_eq!(profile.runner_mode, AgentRunnerMode::Cli);
    }

    #[test]
    fn runner_mode_api_round_trip() {
        let yaml = r#"
id: api-agent
name: Api Agent
description: An agent with runner_mode api.
provider:
  type: Cli
  command: claude
  args: []
model: null
system_prompt: null
tools: null
max_instances: 1
timeout_seconds: 300
serialize: true
runner_mode: api
"#;
        let profile: AgentProfile = serde_yaml::from_str(yaml).expect("deserialize");
        assert_eq!(profile.runner_mode, AgentRunnerMode::Api);
        let serialized = serde_yaml::to_string(&profile).expect("serialize");
        let round_tripped: AgentProfile = serde_yaml::from_str(&serialized).expect("deserialize again");
        assert_eq!(round_tripped.runner_mode, AgentRunnerMode::Api);
    }

    #[test]
    fn default_profile_has_cli_runner_mode() {
        let p = make_profile();
        assert_eq!(p.runner_mode, AgentRunnerMode::Cli);
    }

    #[test]
    fn skills_field_yaml_round_trip() {
        let mut p = make_profile();
        p.skills = vec!["alpha".to_string(), "beta".to_string()];

        let yaml = serde_yaml::to_string(&p).expect("serialize");
        let decoded: AgentProfile = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(decoded.skills, vec!["alpha".to_string(), "beta".to_string()]);

        // Round-trip again to confirm idempotency.
        let yaml2 = serde_yaml::to_string(&decoded).expect("serialize round 2");
        let decoded2: AgentProfile = serde_yaml::from_str(&yaml2).expect("deserialize round 2");
        assert_eq!(decoded2.skills, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn owning_team_id_round_trips_and_is_omitted_when_none() {
        let mut p = make_profile();
        // None -> field is skipped on serialize.
        let json_none = serde_json::to_string(&p).expect("serialize");
        assert!(
            !json_none.contains("owning_team_id"),
            "None should be skipped: {json_none}"
        );

        // Some -> field round-trips.
        p.owning_team_id = Some("team-42".into());
        let json_some = serde_json::to_string(&p).expect("serialize");
        assert!(json_some.contains("owning_team_id"));
        let decoded: AgentProfile = serde_json::from_str(&json_some).expect("deserialize");
        assert_eq!(decoded.owning_team_id.as_deref(), Some("team-42"));
    }

    #[test]
    fn max_delegation_depth_is_omitted_when_none() {
        let p = make_profile();
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(
            !json.contains("max_delegation_depth"),
            "None should be skipped: {json}"
        );
    }

    #[test]
    fn max_delegation_depth_round_trips_through_json_and_yaml() {
        let mut p = make_profile();
        p.max_delegation_depth = Some(5);

        let json = serde_json::to_string(&p).expect("serialize json");
        assert!(json.contains("max_delegation_depth"));
        let from_json: AgentProfile = serde_json::from_str(&json).expect("deserialize json");
        assert_eq!(from_json.max_delegation_depth, Some(5));

        let yaml = serde_yaml::to_string(&p).expect("serialize yaml");
        assert!(yaml.contains("max_delegation_depth"));
        let from_yaml: AgentProfile = serde_yaml::from_str(&yaml).expect("deserialize yaml");
        assert_eq!(from_yaml.max_delegation_depth, Some(5));
    }

    #[test]
    fn pre_k_profile_without_max_delegation_depth_deserializes_cleanly() {
        let yaml = r#"
id: old-agent
name: Old Agent
description: Pre-K profile
provider:
  type: Cli
  command: claude
  args: []
model: null
system_prompt: null
tools: null
max_instances: 1
timeout_seconds: 300
serialize: true
"#;
        let profile: AgentProfile =
            serde_yaml::from_str(yaml).expect("pre-K profile should deserialize");
        assert_eq!(
            profile.max_delegation_depth, None,
            "absent field should deserialize as None"
        );

        let reserialized = serde_yaml::to_string(&profile).expect("serialize");
        assert!(
            !reserialized.contains("max_delegation_depth"),
            "None should be omitted on re-serialize: {reserialized}"
        );
    }

    #[test]
    fn profile_without_channels_or_legacy_telegram_deserializes_with_empty_channels() {
        let yaml = r#"
id: old-agent
name: Old Agent
description: Pre-channels profile
provider:
  type: Cli
  command: claude
  args: []
model: null
system_prompt: null
tools: null
max_instances: 1
timeout_seconds: 300
serialize: true
"#;
        let profile: AgentProfile =
            serde_yaml::from_str(yaml).expect("pre-channels profile should deserialize");
        assert!(
            profile.channels.is_empty(),
            "absent channels and telegram should deserialize as an empty vec"
        );

        let reserialized = serde_yaml::to_string(&profile).expect("serialize");
        assert!(
            !reserialized.contains("telegram"),
            "the legacy field must never be re-emitted: {reserialized}"
        );
    }

    #[test]
    fn legacy_telegram_field_migrates_to_single_channel_binding() {
        let yaml = r#"
id: old-agent
name: Old Agent
description: Pre-channels profile
provider:
  type: Cli
  command: claude
  args: []
model: null
system_prompt: null
tools: null
max_instances: 1
timeout_seconds: 300
serialize: true
telegram:
  enabled: true
  bot_username: "@axew_research_bot"
  thread_mode: dedicated
  bridge_thread_id: bridge-thread-1
  allowed_chat_ids: [123, 456]
"#;
        let profile: AgentProfile =
            serde_yaml::from_str(yaml).expect("legacy telegram field should migrate");

        assert_eq!(profile.channels.len(), 1, "expected exactly one migrated binding");
        let binding = &profile.channels[0];
        assert_eq!(binding.binding_id, "telegram", "migration id must be deterministic");
        assert_eq!(binding.kind, ChannelKind::Telegram);
        assert!(binding.enabled);
        assert_eq!(binding.bridge_thread_id.as_deref(), Some("bridge-thread-1"));
        assert_eq!(binding.allowed_senders, vec!["123".to_string(), "456".to_string()]);
        assert_eq!(binding.pending_pairing_code, None);
        assert_eq!(
            binding.kind_config,
            ChannelKindConfig::Telegram {
                bot_username: Some("@axew_research_bot".to_string()),
                thread_mode: TelegramThreadMode::Dedicated,
            }
        );

        let reserialized = serde_yaml::to_string(&profile).expect("serialize");
        assert!(
            reserialized.contains("channels"),
            "migrated binding must be emitted under channels: {reserialized}"
        );
        assert!(
            !reserialized.contains("telegram:"),
            "the legacy top-level field must never be re-emitted: {reserialized}"
        );
    }

    #[test]
    fn legacy_telegram_without_bridge_thread_id_or_pairing_code_migrates_cleanly() {
        let yaml = r#"
id: old-agent
name: Old Agent
description: Pre-bridge-thread profile
provider:
  type: Cli
  command: claude
  args: []
model: null
system_prompt: null
tools: null
max_instances: 1
timeout_seconds: 300
serialize: true
telegram:
  enabled: true
  thread_mode: dedicated
  allowed_chat_ids: []
"#;
        let profile: AgentProfile = serde_yaml::from_str(yaml)
            .expect("telegram config predating bridge_thread_id/pairing code should deserialize");
        let binding = profile.telegram_binding().expect("migrated binding present");
        assert_eq!(binding.bridge_thread_id, None);
        assert_eq!(binding.pending_pairing_code, None);
        assert!(binding.allowed_senders.is_empty());
    }

    #[test]
    fn channels_profile_with_telegram_binding_round_trips_when_present() {
        let mut p = make_profile();
        p.channels = vec![ChannelBinding {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: true,
            bridge_thread_id: Some("bridge-thread-1".to_string()),
            allowed_senders: vec!["123".to_string(), "456".to_string()],
            pending_pairing_code: Some(PairingCode {
                code: "ABC234".to_string(),
                expires_at_unix: 1_700_000_600,
            }),
            kind_config: ChannelKindConfig::Telegram {
                bot_username: Some("@axew_research_bot".to_string()),
                thread_mode: TelegramThreadMode::Dedicated,
            },
        }];

        let yaml = serde_yaml::to_string(&p).expect("serialize");
        assert!(yaml.contains("channels"));
        let decoded: AgentProfile = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(p, decoded);
    }

    #[test]
    fn channels_profile_with_email_binding_round_trips() {
        let mut p = make_profile();
        p.channels = vec![ChannelBinding {
            binding_id: "email-default".to_string(),
            kind: ChannelKind::Email,
            enabled: true,
            bridge_thread_id: Some("bridge-thread-2".to_string()),
            allowed_senders: vec!["axew@example.com".to_string()],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Email {
                address: "agent-inbox@example.com".to_string(),
                imap_host: "imap.example.com".to_string(),
                imap_port: 993,
                smtp_host: "smtp.example.com".to_string(),
                smtp_port: 587,
                poll_secs: 30,
                require_auth_results: true,
            },
        }];

        let yaml = serde_yaml::to_string(&p).expect("serialize");
        assert!(yaml.contains("email"));
        let decoded: AgentProfile = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(p, decoded);
    }

    #[test]
    fn channels_profile_with_discord_binding_round_trips() {
        let mut p = make_profile();
        p.channels = vec![ChannelBinding {
            binding_id: "discord-default".to_string(),
            kind: ChannelKind::Discord,
            enabled: true,
            bridge_thread_id: Some("bridge-thread-3".to_string()),
            allowed_senders: vec!["123456789012345678".to_string()],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Discord {
                allowed_users: vec!["123456789012345678".to_string()],
                allowed_roles: vec!["987654321098765432".to_string()],
                allowed_channels: vec!["555555555555555555".to_string()],
                dm_role_auth_guild: Some("111111111111111111".to_string()),
                require_mention: false,
                thread_follow: ThreadFollowMode::Always,
                thread_idle_timeout_minutes: 30,
                thread_message_budget: 25,
                backfill_limit: 50,
            },
        }];

        let yaml = serde_yaml::to_string(&p).expect("serialize");
        assert!(yaml.contains("discord"));
        let decoded: AgentProfile = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(p, decoded);
    }

    #[test]
    fn discord_config_json_missing_vec_fields_deserializes_to_empty_defaults() {
        // A hand-authored payload predating `allowed_roles`/`allowed_channels`
        // (or one that simply omits empty arrays) must still deserialize,
        // filling the Vec fields in as empty rather than failing.
        let json = r#"{ "type": "Discord" }"#;
        let config: ChannelKindConfig =
            serde_json::from_str(json).expect("discord config missing vec fields should deserialize");
        assert_eq!(
            config,
            ChannelKindConfig::Discord {
                allowed_users: vec![],
                allowed_roles: vec![],
                allowed_channels: vec![],
                dm_role_auth_guild: None,
                require_mention: true,
                thread_follow: ThreadFollowMode::StickyDecay,
                thread_idle_timeout_minutes: 15,
                thread_message_budget: 10,
                backfill_limit: 20,
            }
        );
    }

    #[test]
    fn discord_config_json_missing_engagement_fields_deserializes_to_documented_defaults() {
        // A profile persisted before `require_mention`/`thread_follow`/
        // `thread_idle_timeout_minutes`/`thread_message_budget`/
        // `backfill_limit` existed must still deserialize, reproducing
        // exactly the always-on-mention-gate, sticky-decay behavior shipped
        // before this field set landed — no migration required.
        let json = r#"{
            "type": "Discord",
            "allowed_users": ["1"],
            "allowed_roles": ["2"],
            "allowed_channels": ["3"],
            "dm_role_auth_guild": "4"
        }"#;
        let config: ChannelKindConfig =
            serde_json::from_str(json).expect("discord config missing engagement fields should deserialize");
        assert_eq!(
            config,
            ChannelKindConfig::Discord {
                allowed_users: vec!["1".to_string()],
                allowed_roles: vec!["2".to_string()],
                allowed_channels: vec!["3".to_string()],
                dm_role_auth_guild: Some("4".to_string()),
                require_mention: true,
                thread_follow: ThreadFollowMode::StickyDecay,
                thread_idle_timeout_minutes: 15,
                thread_message_budget: 10,
                backfill_limit: 20,
            }
        );
    }

    #[test]
    fn telegram_config_view_and_setter_round_trip_through_channels() {
        let mut p = make_profile();
        assert_eq!(p.telegram_config_view(), None);

        let telegram = TelegramConfig {
            enabled: true,
            bot_username: Some("@axew_research_bot".to_string()),
            thread_mode: TelegramThreadMode::Dedicated,
            bridge_thread_id: Some("bridge-thread-1".to_string()),
            allowed_chat_ids: vec![123, 456],
            pending_pairing_code: Some(PairingCode {
                code: "ABC234".to_string(),
                expires_at_unix: 1_700_000_600,
            }),
        };
        p.set_telegram_config(Some(telegram.clone()));
        assert_eq!(p.channels.len(), 1);
        assert_eq!(p.telegram_config_view(), Some(telegram));

        p.set_telegram_config(None);
        assert!(p.channels.is_empty());
        assert_eq!(p.telegram_config_view(), None);
    }

    #[test]
    fn pairing_code_generate_has_expected_length_and_alphabet() {
        let code = PairingCode::generate(1_700_000_000);
        assert_eq!(code.code.chars().count(), PAIRING_CODE_LENGTH);
        assert!(
            code.code.chars().all(|c| PAIRING_CODE_ALPHABET.contains(c)),
            "every character must come from the pairing code alphabet: {}",
            code.code
        );
        assert_eq!(code.expires_at_unix, 1_700_000_600);
    }

    #[test]
    fn pairing_code_is_expired_at_and_after_expiry_boundary() {
        let code = PairingCode {
            code: "ABC234".to_string(),
            expires_at_unix: 1_700_000_600,
        };
        assert!(
            code.is_expired(1_700_000_600),
            "code should be treated as expired exactly at its expiry instant"
        );
        assert!(code.is_expired(1_700_000_601));
        assert!(!code.is_expired(1_700_000_599));
    }
}

#[cfg(test)]
mod delegate_target_tests {
    use super::*;

    #[test]
    fn pre_h_profile_without_delegates_to_round_trips_cleanly() {
        // A profile YAML from before `delegates_to` existed must deserialise
        // with delegates_to == [] and re-serialise without emitting the field at all.
        let yaml = r#"
id: old-agent
name: Old Agent
description: Pre-H profile
provider:
  type: Cli
  command: claude
  args: []
model: null
system_prompt: null
tools: null
max_instances: 1
timeout_seconds: 300
serialize: true
"#;
        let profile: AgentProfile =
            serde_yaml::from_str(yaml).expect("pre-H profile should deserialize");
        assert!(
            profile.delegates_to.is_empty(),
            "delegates_to should default to empty"
        );

        let reserialized = serde_yaml::to_string(&profile).expect("serialize");
        assert!(
            !reserialized.contains("delegates_to"),
            "empty delegates_to should be omitted from serialized YAML: {reserialized}"
        );
    }

    #[test]
    fn delegate_target_share_context_allowed_defaults_false() {
        let yaml = r#"
target_agent_id: agent-b
name: Agent B
purpose: Handle sub-tasks
"#;
        let target: DelegateTarget =
            serde_yaml::from_str(yaml).expect("DelegateTarget should deserialize");
        assert!(!target.share_context_allowed);
    }
}

#[cfg(test)]
mod launchpad_convention_skills_tests {
    use super::*;

    #[test]
    fn legacy_profile_without_launchpad_skill_fields_round_trips_cleanly() {
        // Existing on-disk agent profile JSON predates the convention-folder
        // skill fields — it must deserialize with both taking their defaults
        // and re-serialize without emitting either field.
        let json = r#"{
            "id": "legacy",
            "name": "Legacy",
            "description": "An agent from before convention-folder skills existed.",
            "provider": { "type": "Cli", "command": "claude", "args": [] },
            "model": null,
            "system_prompt": null,
            "tools": null,
            "max_instances": 1,
            "timeout_seconds": 300,
            "serialize": true
        }"#;
        let profile: AgentProfile =
            serde_json::from_str(json).expect("legacy profile should deserialize");
        assert_eq!(profile.enabled_launchpad_global_skills, None);
        assert!(profile.enabled_launchpad_project_skills.is_empty());

        let reserialized = serde_json::to_string(&profile).expect("serialize");
        assert!(
            !reserialized.contains("enabled_launchpad_global_skills"),
            "None should be omitted on re-serialize: {reserialized}"
        );
        assert!(
            !reserialized.contains("enabled_launchpad_project_skills"),
            "empty map should be omitted on re-serialize: {reserialized}"
        );

        let round_tripped: AgentProfile =
            serde_json::from_str(&reserialized).expect("round-trip deserialize");
        assert_eq!(round_tripped.enabled_launchpad_global_skills, None);
        assert!(round_tripped.enabled_launchpad_project_skills.is_empty());
    }

    #[test]
    fn canonical_project_key_strips_trailing_slash_for_nonexistent_path() {
        let path = "/tmp/launchpad-canonical-key-test-does-not-exist-xyz/";
        let key = canonical_project_key(path);
        assert_eq!(key, "/tmp/launchpad-canonical-key-test-does-not-exist-xyz");
    }

    #[test]
    fn canonical_project_key_is_deterministic() {
        let path = "/tmp/launchpad-canonical-key-test-does-not-exist-xyz/";
        assert_eq!(canonical_project_key(path), canonical_project_key(path));
    }
}

#[cfg(test)]
mod max_turns_tests {
    use super::*;

    /// (c) Existing on-disk agent profile JSON that predates `max_turns`
    /// must still deserialize — the field takes `None` (deferring to
    /// [`DEFAULT_MAX_TURNS`] at the call site) rather than failing to parse
    /// or silently dropping the rest of the profile.
    #[test]
    fn legacy_profile_without_max_turns_round_trips_cleanly() {
        let json = r#"{
            "id": "legacy",
            "name": "Legacy",
            "description": "An agent from before the turn cap existed.",
            "provider": { "type": "Cli", "command": "claude", "args": [] },
            "model": null,
            "system_prompt": null,
            "tools": null,
            "max_instances": 1,
            "timeout_seconds": 300,
            "serialize": true
        }"#;
        let profile: AgentProfile =
            serde_json::from_str(json).expect("legacy profile should deserialize");
        assert_eq!(profile.max_turns, None);

        let reserialized = serde_json::to_string(&profile).expect("serialize");
        assert!(
            !reserialized.contains("max_turns"),
            "None should be omitted on re-serialize: {reserialized}"
        );

        let round_tripped: AgentProfile =
            serde_json::from_str(&reserialized).expect("round-trip deserialize");
        assert_eq!(round_tripped.max_turns, None);
    }

    /// Pins the fallback value itself — a regression here means every
    /// profile that has never set `max_turns` silently gets a different
    /// budget than the one the owner signed off on.
    #[test]
    fn default_max_turns_is_fifty() {
        assert_eq!(DEFAULT_MAX_TURNS, 50);
    }

    #[test]
    fn explicit_max_turns_round_trips() {
        let json = r#"{
            "id": "capped",
            "name": "Capped",
            "description": "An agent with an explicit turn cap.",
            "provider": { "type": "Cli", "command": "claude", "args": [] },
            "model": null,
            "system_prompt": null,
            "tools": null,
            "max_instances": 1,
            "timeout_seconds": 300,
            "serialize": true,
            "max_turns": 15
        }"#;
        let profile: AgentProfile =
            serde_json::from_str(json).expect("profile with max_turns should deserialize");
        assert_eq!(profile.max_turns, Some(15));
    }
}
