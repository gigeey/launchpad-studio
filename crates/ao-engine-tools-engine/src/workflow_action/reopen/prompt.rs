pub const DESCRIPTION: &str = "Move a terminal workflow task back to a specific phase for re-run \
while preserving all existing output files. Valid source states are Completed, Failed, and \
Stopped — you cannot reopen a task that is currently Pending or Running. On success the task \
status returns to Pending and the requested phase is reset so the workflow runner will \
re-execute it; all phases before the rewind point keep their Completed/Skipped state and all \
output files remain intact. Use this when you want to re-run one phase with adjusted inputs \
without losing work from prior phases. If the requested phase_id is invalid the error message \
lists all valid phase IDs from the workflow definition.";
