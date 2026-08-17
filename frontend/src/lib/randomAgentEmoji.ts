const AGENT_EMOJI_POOL = [
    "🤖", "🦾", "🧠", "🛸", "🚀", "✨", "🌟", "⚡", "🔮", "🎯",
    "🦊", "🦉", "🦁", "🐼", "🐙", "🦄", "🐝", "🦋", "🐬", "🦅",
    "🧙", "🧑‍🚀", "🧑‍💻", "🕵️", "🦸", "🧚", "🧞", "🥷",
    "📚", "🧭", "🔧", "🧰", "🧩", "🎨", "🎼", "📡",
];

export function randomAgentEmoji(): string {
    const idx = Math.floor(Math.random() * AGENT_EMOJI_POOL.length);
    return AGENT_EMOJI_POOL[idx];
}
