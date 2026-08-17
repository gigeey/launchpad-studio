pub const DESCRIPTION: &str =
    "Permanently delete a workflow task by its ID. The on-disk task directory \
     (including all phase outputs) is removed and cannot be recovered. Use this \
     to clean up abandoned or completed tasks that no longer need to be retained. \
     Prefer leaving completed tasks in place when their outputs may still be \
     referenced. Returns a recoverable error if the task ID is not found.";
