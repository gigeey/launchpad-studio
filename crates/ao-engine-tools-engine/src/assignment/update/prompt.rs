pub const DESCRIPTION: &str =
    "Update an existing assignment by id. Every field besides assignment_id is optional — \
     only the fields you pass are changed, everything else is left as-is. Passing a new \
     trigger fully replaces the old one (schedule cron expressions are re-validated and \
     next_fire_at is recomputed). Use enabled=false to pause an assignment without \
     deleting it. Restricted to the top-level agent — subagents cannot update assignments.";
