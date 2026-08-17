pub const DESCRIPTION: &str =
    "Read the persisted state snapshot for a workflow task. Returns the task status \
     (pending / running / completed / etc.), the per-phase status map, and metadata. \
     \
     Use this to diagnose a workflow that looks wrong. The most common failure mode: \
     output files exist in the task's output directory but the phases map is empty or \
     all-pending. That means the files were written through the generic Write tool \
     instead of WorkflowActionWriteOutput — the workflow has no record of them and \
     the phases will never advance on their own. The fix is to re-write each file \
     through WorkflowActionWriteOutput and then call WorkflowActionCompletePhase per \
     phase in declaration order.";
