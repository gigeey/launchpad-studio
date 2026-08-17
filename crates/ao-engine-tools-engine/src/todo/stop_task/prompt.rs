use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Halt a single InProgress task without touching any sibling tasks.

TodoStopTask transitions the target task from InProgress to Stopped, clears
its assignment, and bumps the classifier token so any in-flight classifier
CAS is rejected. In SEQ groups the stopped task blocks all tasks behind it
until it is resumed; in PAR groups siblings keep running unaffected.

The stopped task stays in place — its position in the list is preserved.
Use TodoResumeTask to return it to Pending so the feeder re-dispatches it.

If the underlying runner is already dead the call degrades safely: the
runner's eventual Completed or Failed outcome will transition the task out
of Stopped normally. Use TodoList to confirm status before calling.

State transition: InProgress → Stopped
Only valid on an InProgress task; rejects any other status.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "ID of the InProgress task to stop."
            }
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}
