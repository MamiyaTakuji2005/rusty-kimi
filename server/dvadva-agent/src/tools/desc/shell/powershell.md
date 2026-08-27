Execute a ${SHELL} command. Use it for scripts, builds, tests, process management, and anything without a dedicated tool.

- Every call runs in a fresh session: environment variables, `Set-Location`, and history do not carry over between calls.
- Put related work in one call with `&&`, `;`, `if ($?)`, pipes, and redirects, rather than making several.
- The call returns only when the command exits, so never start an interactive or unbounded command, and set `timeout` for slow ones.
- stdout and stderr come back combined and may be truncated; a failed command reports its exit code in a system tag.
- For reading, writing, searching, and editing files, prefer ReadFile, WriteFile, StrReplaceFile, Grep, and Glob.
- Stay inside the working directory, and do not use elevated privileges unless the user asked for it.
