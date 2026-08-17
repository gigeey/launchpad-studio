pub const DESCRIPTION: &str =
    "Advance a workflow phase to the completed state. Every output file declared by \
     the phase must first have been written through WorkflowActionWriteOutput — the \
     generic Write tool will not satisfy this requirement, because direct file writes \
     are invisible to the workflow state machine. Returns an error if any declared \
     output is missing. \
     \
     When pre-filling multiple phases on a still-pending task, complete them in \
     declaration order: completing a downstream phase before its dependency is \
     rejected. \
     \
     Typical pre-fill sequence: WorkflowActionWriteOutput (once per output file) → \
     WorkflowActionCompletePhase (this tool, once per pre-filled phase, in order) → \
     WorkflowActionStart to transition the task into the running state.";
