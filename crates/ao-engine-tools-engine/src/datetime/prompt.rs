pub const DESCRIPTION: &str =
    "Read the current wall-clock time. Returns UTC and local-timezone timestamps \
     in ISO 8601 plus the Unix epoch. Use this whenever you need to compute a \
     `next_fire_at` for a scheduled task, reason about how recent something is, \
     or anchor relative dates (\"yesterday\", \"in 30 minutes\") against an \
     authoritative now. Has no parameters.";
