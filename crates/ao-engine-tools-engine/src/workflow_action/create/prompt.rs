pub const DESCRIPTION: &str =
    "Create a new workflow task bound to the given workflow. The agent must be bound \
     to the workflow (via its WorkflowBinding) to call this tool. Returns the task ID, \
     output directory, and the list of declared phases. \
     \
     A freshly created task is in the pending state — phases have not begun running. \
     Two ways to proceed from here: \
     \
     1. Pre-fill, then start. When prior conversation already covers one or more early \
        phases, write each declared output of those phases through \
        WorkflowActionWriteOutput and mark each phase complete through \
        WorkflowActionCompletePhase (in declaration order) before calling \
        WorkflowActionStart. This is the right path when the user says \"pre-fill the \
        interview\" or similar. \
     \
     2. Clean start. Call WorkflowActionStart directly. The first phase begins \
        immediately. \
     \
     Writing files into the task's output directory via the generic Write tool does \
     NOT pre-fill the workflow — the state machine has no record of those files and \
     the corresponding phases will still need to run.";
