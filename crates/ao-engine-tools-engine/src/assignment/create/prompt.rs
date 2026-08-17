pub const DESCRIPTION: &str =
    "Create an assignment: a standing rule that runs an instruction whenever its trigger \
     fires. Give it a name, the instruction to run, and a trigger — either a schedule \
     (cron_expr, optionally one-shot via is_recurring=false), a webhook (optionally token-\
     protected), or a connector_event (poll a connector tool on a timer and fire only when \
     it reports something new: set server_name, a poll object with tool_name/arguments/\
     cursor_path, and poll_interval_secs. The first poll seeds a baseline without firing, \
     then each later poll fires when the value at cursor_path changes). thread_policy \
     controls which thread each run lands in and defaults from the trigger type when \
     omitted: schedule defaults to \"main\" (feels like a reminder), while webhook and \
     connector_event default to \"fresh\" (never interrupts a live chat). Optionally \
     set working_directory, expires_at, bindings (connectors the run may use), or agent_id \
     to create on behalf of a different agent. Restricted to the top-level agent — \
     subagents cannot create assignments.";
