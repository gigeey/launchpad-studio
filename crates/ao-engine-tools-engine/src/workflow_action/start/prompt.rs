pub const DESCRIPTION: &str =
    "Transition a pending workflow task into the running state. The workflow's queue \
     manager advances the task to the first incomplete phase (skipping any phases \
     pre-completed via WorkflowActionWriteOutput + WorkflowActionCompletePhase) and \
     begins executing. \
     \
     Pre-fill workflow — when conversation context already covers one or more early \
     phases: create the task → write each declared output of those phases via \
     WorkflowActionWriteOutput → complete each pre-filled phase via \
     WorkflowActionCompletePhase (in declaration order) → confirm with the user → \
     call this tool. \
     \
     Clean start — when no pre-fill content exists: create the task → call this tool. \
     The first phase begins immediately. \
     \
     Calling this on a task that still has unwritten outputs for a pre-fill phase will \
     simply run that phase from scratch — the previously-written file content will be \
     overwritten by the phase agent.";
