Stop a running background task.

- Stopping is destructive and may leave partial side effects, so use it only when a task must be cancelled.
- For normal completion, wait for the task to announce itself or read it with TaskOutput.
- A task that has already finished simply returns its current state.
