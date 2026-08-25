Retrieve the output of a running or finished background task.

- Non-blocking by default: it returns a snapshot of the task's current status and output.
- Set `block=true` only when you deliberately want to wait for completion or timeout.
- A finished task announces itself, so reach for this when you need its output sooner than that.
