pub const DESCRIPTION: &str = "Pause the agent for a specified number of seconds. \
Use this to introduce a deliberate delay — for example, to wait for an external process \
to complete, to pace an automated polling loop, or to avoid sending requests in rapid \
succession.\n\n\
If the user sends input while the timer is running, the pause ends immediately: the \
tool returns an interrupted result and the agent resumes its turn as usual.\n\n\
Choose this over a shell-level sleep (e.g. `Bash(sleep N)`) whenever the wait would \
outlive a single shell command — no subprocess stays parked for the duration, and \
cancellation takes effect right away.\n\n\
Minimum: 1 second. Maximum: 3600 seconds (1 hour).";
