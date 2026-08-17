# Telegram Bridge — Setup & Usage

Connect a Launchpad agent to a Telegram bot so you can DM the bot, have the agent
run against the message, and get the reply back in the same chat — a full round trip.

> **The one thing that trips everyone up:** an agent with an **empty allow-list
> rejects every message** (reject-all security model). You must **link your chat
> first** (`/start <code>`). A normal message sent before linking is silently
> dropped and will never appear in the thread.

---

## 1. Create a Telegram bot (one time)

1. In Telegram, DM [@BotFather](https://t.me/BotFather).
2. Send `/newbot`, follow the prompts (display name + username ending in `bot`).
3. BotFather returns a **bot token** like `123456789:AAExxxxxxxxxxxxxxxxxxxxxxxxxxx`.
   Keep it — you'll paste it into the app.

## 2. Connect the bot to an agent

In the Launchpad app:

1. Open the agent → **Telegram settings** (in the agent profile / settings panel).
2. Paste the **bot token**.
3. **Save.** ← Do not skip Save. Saving is what provisions the bridge thread that
   actually receives messages (see Troubleshooting → "bot ignores `/start`").
4. Confirm it shows **enabled** and displays the bot's `@username` (the app calls
   Telegram's `getMe` to verify the token).

Once saved and enabled, the bridge supervisor begins long-polling Telegram for
this bot every few seconds.

## 3. Connecting Telegram (DMs & Groups)

This is what puts a chat on the agent's linked-chats allow-list. Until a chat is
linked, the bridge silently drops every message it sends (server log line:
`TelegramTransport: dropping message from unlinked chat`) — that's the expected
anti-spam gate, not a bug.

> [!IMPORTANT]
> **Every chat needs its own pairing code.** Each DM and each group is paired
> **separately**, using a **freshly generated code each time** — codes are
> **single-use** and **expire after 10 minutes**. Pairing a new chat only *adds*
> to the agent's linked-chats list; it never overwrites or removes chats you
> already paired. Link a DM today and a group next week — both keep working.

### Pair a DM

1. In the agent's Telegram settings → **Generate pairing code**. A short code is
   shown (with an expiry).
2. In Telegram, DM your bot: **`/start <code>`** (e.g. `/start 4F9K2A`).
3. The bot replies:
   > *You're linked. I'll respond to messages in this chat from now on.*

### Pair a group

1. Add the bot to the group.
2. **Turn off Group Privacy for the bot (one-time, per bot):** in
   [@BotFather](https://t.me/BotFather) → `/mybots` → select your bot →
   **Bot Settings** → **Group Privacy** → **Turn off**. Without this, the bot
   only ever sees `/commands` in the group and can't see normal messages —
   which means it can't detect @mentions either.
3. In the agent's Telegram settings → **Generate a new pairing code** (don't
   reuse a code from a DM or another group — codes are single-use).
4. In the group, send **`/start <code>`**. If more than one bot shares the
   group, send **`/start@yourbot <code>`** instead, so the right bot claims it.
5. The bot confirms it's linked to the group.

Repeat "generate a new code → `/start <code>`" for every additional DM or group
you want the agent to respond in. Each paired chat appears in the agent's
**linked chats** list in settings (you can unlink any of them there anytime),
and unlinking one chat has no effect on the others.

## 4. Use it

Send any normal message to the bot. It routes into the agent's thread → the agent
runs → the reply is relayed back to your Telegram chat. Long replies are split
into multiple messages automatically.

---

## How it works (mechanism)

1. **Bridge supervisor** wakes on a short interval (~5s) and, for each
   Telegram-enabled agent, long-polls Telegram's `getUpdates`.
2. Each inbound update is filtered against the agent's linked-chats allow-list
   (keyed per agent + bot binding, holds many chat ids at once).
   - `/start <valid code>` → that chat id is **appended** to the allow-list
     (deduped). Existing entries are never removed or overwritten by pairing a
     new chat.
   - **Empty allow-list ⇒ reject all** (nothing is trusted by default).
3. An allowed message is submitted to the agent's **bridge thread**; the agent runs.
4. On turn completion, the agent's reply is **relayed back** to the originating
   chat (correlated via an in-flight `thread_id → chat_id` side-map).

Tokens are stored separately (OS keychain, with a `telegram_tokens.json` fallback),
not in the agent profile JSON.

---

## Troubleshooting

### Nothing arrives at all — bridge isn't polling (stale server)

*This one only affects you if you run from source ([DEVELOPING.md](DEVELOPING.md)).
If you use the packaged app, skip to the next section.*

Most common cause: the running `ao-server` process started **before** the current
binary was built, so it's serving an older image without the bridge.

**Detect (read-only, macOS):** from the repo root, with `AO_PORT` set to whatever
your server binds (`3001` by default):

```bash
ps -o pid,lstart,command -p "$(lsof -tiTCP:${AO_PORT:-3001} -sTCP:LISTEN)"  # process START time
stat -f "on-disk binary mtime=%Sm" target/debug/ao-server                   # binary BUILD time
```
If the binary's mtime is **newer** than the process's start time, the live server
is stale and must be restarted onto the fresh binary.

**Restart:**
```bash
cargo build --bin ao-server
kill -TERM "$(lsof -tiTCP:${AO_PORT:-3001} -sTCP:LISTEN)"
nohup target/debug/ao-server > /tmp/ao-server.log 2>&1 &
disown
```
> Note: `ao-server` is a **shared** dev server — restarting it drops in-flight
> background agents. If other agents/threads may be running against it, restart at
> a quiet moment (or if you launch via `tauri dev`, restart that way).

Confirm the bridge came up:
```bash
tail -f /tmp/ao-server.log | grep -i telegram
# expect: "TelegramBridge starting" + "starting long-poll task"
```

### Messages sent, but they never reach the thread

You almost certainly **haven't linked the chat**. Empty allow-list = reject-all.
Generate a pairing code and DM `/start <code>` first (Section 3), *then* message.

### Bot ignores `/start` even though it's enabled

Known gap: pasting a token and closing settings **without Save** can leave the
agent `enabled=true` but with **no provisioned bridge thread**, so the poll task
self-stops (`bridge thread not yet provisioned`). **Re-open settings and Save the
token** to provision the thread.

### Order matters

Linking only works once the bridge is live. If you restarted the server:
**restart first, then `/start`.**

---

## Quick reference

| Step | Where | Action |
|---|---|---|
| Get token | @BotFather in Telegram | `/newbot` → copy token |
| Connect | App → agent → Telegram settings | Paste token → **Save** → enable |
| Group setup (groups only, one-time per bot) | @BotFather → `/mybots` → Bot Settings | **Group Privacy** → **Turn off** |
| Pair *each* chat | App → Telegram settings | **Generate a new pairing code** (single-use, 10 min expiry) |
| Link *each* chat | Telegram (the DM or group) | `/start <code>` (or `/start@yourbot <code>` in shared groups) → "You're linked." |
| Use | Telegram DM or group | Send a normal message |
| Unlink | App → Telegram settings | Remove that chat from the linked list (others unaffected) |
