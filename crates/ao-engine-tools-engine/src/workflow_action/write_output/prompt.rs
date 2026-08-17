pub const DESCRIPTION: &str =
    "Persist a phase output file AND register it with the workflow state machine. \
     This is the only path that attaches a file to a phase — writing the same file \
     through the generic Write tool puts bytes on disk but leaves the workflow's phase \
     tracker untouched, so the phase will never count as complete. \
     If a workflow ever appears to have an empty phases map despite files existing on \
     disk, that is the symptom: the files were written through the wrong tool. \
     \
     Arguments: task_id, filename (e.g. 'analysis.json'), and content (the content goes \
     verbatim to disk — JSON outputs must be pre-serialized strings). \
     \
     After every declared output for a phase has been written through this tool, call \
     WorkflowActionCompletePhase to advance the phase. \
     \
     Schema rule for ralph-style prd.json: every userStory must be written with \
     passes:false. The passes flag flips to true only after the implementation phase \
     verifies the acceptance criteria — writing passes:true at PRD-creation time is \
     rejected.";
