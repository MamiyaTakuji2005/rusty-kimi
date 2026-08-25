Run a separate agent process on a task and return its text output.

- The child starts with a fresh session and its own tool access, and knows nothing about this conversation.
- Give it explicit goals, the paths it needs, and what finished looks like.
- Prefer doing the work yourself unless it genuinely benefits from isolation, or several tasks can run in parallel.
${AGENTS}