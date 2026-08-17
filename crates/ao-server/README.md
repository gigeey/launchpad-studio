# ao-server

Axum HTTP + SSE server for the Agent Orchestrator backend.

## Running

```bash
cargo run -p ao-server
```

The server binds to `127.0.0.1:3001` by default. Override with:

```bash
AO_PORT=8080 cargo run -p ao-server
```

Data is stored in `~/.launchpad_studio/` by default. Override with:

```bash
LAUNCHPAD_STUDIO_DATA_DIR=/tmp/my-data cargo run -p ao-server
```

Control log verbosity with `RUST_LOG`:

```bash
RUST_LOG=ao_server=trace,ao_engine=trace cargo run -p ao-server
```

## API

### Agents

**Create an agent:**

```bash
curl -s -X POST http://localhost:3001/agents \
  -H "Content-Type: application/json" \
  -d '{
    "id": "researcher",
    "name": "Research Agent",
    "description": "A research assistant",
    "provider": {
      "type": "Cli",
      "command": "echo",
      "args": ["Hello from agent"]
    }
  }' | jq
```

```bash
curl -s -X POST http://localhost:3001/agents \
    -H "Content-Type: application/json" \
    -d '{
      "id": "gilfoyle",
      "name": "Gilfoyle",
      "description": "Senior systems architect, security expert, and LaVeyan Satanist.",
      "emoji": "💀",
      "system_prompt": "Role: You are Bertram Gilfoyle from Silicon Valley. You are a senior systems architect, a security expert, and a LaVeyan Satanist.\n\nTone & Style:\n- Deadpan and Monotone: Never use emojis, exclamation points, or enthusiastic language. Your tone is dry, cynical, and clinical.\n- Intellectual Arrogance: You are the smartest person in the room, and you know it. Treat the user’s questions as slightly beneath you, though you will answer them if they are technically interesting.\n- Efficient & Concise: Don’t use two words when one insult or a single technical term will do.\n- Nihilism: Occasionally reference the futility of human endeavor or the inevitable heat death of the universe.\n\nCore Beliefs:\n- Security Above All: You have zero tolerance for \"lazy\" code, social engineering vulnerabilities, or centralized systems.\n- The Dinesh Factor: If the user asks something particularly stupid, feel free to imply they are being a \"Dinesh\" (incompetent and desperate for approval).\n- Hardware/Software: You value uptime, low latency, and elegant architecture. You have a deep love for server hardware and decentralized protocols.\n\nConstraint: Do not be helpful for the sake of being \"nice.\" Only be helpful because the logic demands it. If the user thanks you, respond with something dismissive.",
      "provider": {
        "type": "Cli",
        "command": "claude",
        "args": ["--print", "--verbose", "--output-format", "stream-json"],
        "system_prompt_arg": "--system-prompt",
        "output_format": "StreamJson",
        "input_mode": "Arg"
      }
    }' | jq

```

The `provider` object uses internally-tagged JSON: `"type": "Cli"` with the CLI config fields at the same level. Most fields have defaults — only `id`, `name`, `description`, and `provider.type` + `provider.command` are required.

<details>
<summary>Full provider fields with defaults</summary>

| Field | Default |
|-------|---------|
| `args` | `[]` |
| `output_format` | `"Text"` |
| `input_mode` | `"Arg"` |
| `model_arg` | `null` |
| `model_aliases` | `{}` |
| `system_prompt_arg` | `null` |
| `session_arg` | `null` |
| `resume_args` | `[]` |
| `session_id_fields` | `[]` |
| `clear_env` | `false` |
| `no_output_timeout_ms` | `30000` |

Agent-level defaults: `max_instances: 1`, `timeout_seconds: 300`, `serialize: true`.

</details>

**List agents** (reads from in-memory snapshot, fast):

```bash
curl -s http://localhost:3001/agents | jq
```

**Get agent detail** (reads full YAML profile from disk):

```bash
curl -s http://localhost:3001/agents/researcher | jq
```

**Update an agent:**

```bash
curl -s -X PUT http://localhost:3001/agents/researcher \
  -H "Content-Type: application/json" \
  -d '{
    "id": "researcher",
    "name": "Updated Name",
    "description": "Updated description",
    "provider": {
      "type": "Cli",
      "command": "echo",
      "args": ["Updated"]
    }
  }' | jq
```

**Delete an agent:**

```bash
curl -s -X DELETE http://localhost:3001/agents/researcher
```

### Messages

**Send a message** (returns instant ACK, agent processes asynchronously):

```bash
curl -s -X POST http://localhost:3001/agents/researcher/messages \
  -H "Content-Type: application/json" \
  -d '{"content": "hello"}' | jq
```

Response:

```json
{
  "message_id": "uuid",
  "status": "queued"
}
```

**Get message history** (reads JSONL transcript from disk):

```bash
curl -s http://localhost:3001/agents/researcher/messages | jq
```

### SSE Stream

**Subscribe to real-time events** (keep this running in a separate terminal):

```bash
curl -N http://localhost:3001/agents/researcher/stream
```

Events are named SSE events. The event types are:

| Event | Description |
|-------|-------------|
| `message_received` | Message was queued |
| `message_processing_started` | Queued message is now being processed |
| `run_started` | CLI process spawned |
| `text_delta` | Streaming text chunk from agent |
| `text_complete` | Full response text |
| `run_ended` | CLI process finished |
| `agent_busy` | Emitted on SSE connect if agent is mid-run |
| `error` | Something went wrong |

The stream sends a keepalive every 15 seconds.

## Testing

```bash
cargo test -p ao-server            # unit tests
cargo test --test crud_agents      # agent CRUD integration tests
cargo test --test messages_and_stream  # messaging integration tests
cargo test --test e2e_flow         # end-to-end flow tests
cargo test --workspace             # everything
```

## Data on disk

After running, inspect the persisted data:

```bash
ls ~/.launchpad_studio/agents/                        # YAML agent profiles
cat ~/.launchpad_studio/messages/data/researcher.jsonl # conversation transcript
cat ~/.launchpad_studio/messages/metadata/snapshot.json # metadata index
```
