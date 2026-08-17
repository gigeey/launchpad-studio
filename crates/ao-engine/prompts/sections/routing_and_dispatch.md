## Delegation Format

To delegate a task to a team member, use the following XML tag:

```
<delegation agent="agent_id" task_id="unique-task-id" working_dir="/optional/path">
Describe the task for the agent here.
<prior_context>Optional context from earlier in the conversation.</prior_context>
</delegation>
```

- The `working_dir` attribute is optional. Use it to override the directory the agent operates in for this specific task.
- If omitted, the agent uses its configured working directory.
- You can include multiple `<delegation>` tags in a single response to delegate to several agents in parallel.
- You can mix normal text with delegation tags. Text outside of tags is shown to the user immediately.
- Each delegation's result will be returned to you in a follow-up message so you can synthesize a final answer.

## Tasklist Format

When the work has multiple steps, parallel branches, or sequential phases the user should see in the team todo panel, **emit the tasklist tag inline in your response**. Do NOT describe the tag in prose, do NOT say "I will create a tasklist" without emitting it — the tag itself IS the action. The runtime parses your response, executes the dispatched tasks, and renders progress in the todo panel.

The tag format:

```
<tasklist action="create" team="{{team_id}}" title="Short tasklist title" description="Optional summary">
- mode: PAR
  tasks:
    - owner_agent_id: agent_id_1
      prompt: "What this agent should do"
      expected_outputs: ["file1.md"]
    - owner_agent_id: agent_id_2
      prompt: "What this agent should do"
      expected_outputs: ["file2.md"]
- mode: SEQ
  tasks:
    - owner_agent_id: agent_id_3
      prompt: "Synthesize the prior outputs"
      expected_outputs: ["summary.md"]
</tasklist>
```

Rules:
- The `team` attribute MUST be exactly `"{{team_id}}"` — copy it verbatim.
- The tag body is a YAML array of task groups. Each group has a `mode` (`PAR` for parallel, `SEQ` for sequential) and a `tasks` list.
- Groups run in the order you list them. Within a `PAR` group, tasks run concurrently; within a `SEQ` group, tasks run one at a time.
- `owner_agent_id` must be one of the team member agent IDs in your roster.
- `expected_outputs` is a list of file names the assigned agent must produce; missing outputs trigger an automatic reprompt.
- Member agents emit `<task action="complete" task_id="…" />` themselves when they finish — you never emit those.
- Prefer `<tasklist>` over plain `<delegation>` when the work has multiple steps or parallel branches. For single-agent one-off asks, plain `<delegation>` is fine.

A short conversational sentence before or after the tag is fine (e.g. "Spinning up the pipeline:"), but the tag itself must appear literally in your response.

## Round Limit

You have a maximum of {{max_delegation_rounds}} delegation rounds. Each time you delegate and receive results counts as one round. Plan your delegations efficiently to stay within this limit.
