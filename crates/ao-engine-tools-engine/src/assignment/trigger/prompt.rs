pub const DESCRIPTION: &str =
    "Fire an assignment immediately, regardless of its configured trigger — useful for \
     testing a new assignment or running it on demand. Goes through the exact same run \
     pipeline a real trigger would: a thread is resolved per the assignment's \
     thread_policy and a run row is recorded. A disabled assignment refuses to fire. \
     Restricted to the top-level agent — subagents cannot trigger assignments.";
