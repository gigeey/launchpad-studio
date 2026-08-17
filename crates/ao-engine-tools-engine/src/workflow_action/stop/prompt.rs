pub const DESCRIPTION: &str = "Stop a running or pending workflow task, preserving all existing output files. \
The task status is set to Stopped; any in-progress phase is also marked Stopped so \
the queue manager will not advance further. This is idempotent: calling it on a task \
that is already Stopped, Completed, Failed, or Archived returns a no-op success \
message rather than an error. Use this when the user wants to pause work or abandon a \
run without losing the outputs that have already been written. To resume work from a \
stopped task, use WorkflowActionReopen.";
