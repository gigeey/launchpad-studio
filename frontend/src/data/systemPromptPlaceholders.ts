export type SystemPromptPlaceholder = {
    id: string;
    label: string;
    description: string;
};

export const PLACEHOLDERS: SystemPromptPlaceholder[] = [
    {
        id: "agent_name",
        label: "Agent name",
        description: "The agent's display name from its profile.",
    },
    {
        id: "agent_description",
        label: "Agent description",
        description: "The short description of the agent from its profile.",
    },
    {
        id: "user_name",
        label: "User name",
        description: "The user's full name (for formal or legal contexts).",
    },
    {
        id: "preferred_name",
        label: "Preferred name",
        description: "The name the user prefers to be called.",
    },
    {
        id: "timezone",
        label: "Timezone",
        description: "The user's current timezone (e.g. America/Los_Angeles).",
    },
    {
        id: "current_date",
        label: "Current date",
        description: "Today's date at runtime.",
    },
    {
        id: "agent_memory",
        label: "Agent memory",
        description: "The agent's persisted memory block, injected at runtime.",
    },
    {
        id: "memory_save_instruction",
        label: "Memory save instruction",
        description: "Guidance for the agent on when and how to save to memory.",
    },
    {
        id: "recall_history_instruction",
        label: "Recall history instruction",
        description: "Guidance for the agent on when to recall prior conversation history.",
    },
    {
        id: "workflows",
        label: "Workflows",
        description: "The workflow action instructions and list of available workflows for this agent.",
    },
];

const PLACEHOLDER_IDS = new Set(PLACEHOLDERS.map((p) => p.id));
const PLACEHOLDER_BY_ID = new Map(PLACEHOLDERS.map((p) => [p.id, p]));

export function isKnownPlaceholder(id: string): boolean {
    return PLACEHOLDER_IDS.has(id);
}

export function getPlaceholder(id: string): SystemPromptPlaceholder | undefined {
    return PLACEHOLDER_BY_ID.get(id);
}

export const PLACEHOLDER_REGEX = new RegExp(
    `\\{\\{(${PLACEHOLDERS.map((p) => p.id).join("|")})\\}\\}`,
    "g",
);
