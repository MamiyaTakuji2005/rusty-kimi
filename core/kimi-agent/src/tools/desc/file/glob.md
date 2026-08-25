Find files and directories by glob pattern.

- The walk is ignore-aware: it respects `.gitignore` and skips hidden files and heavy directories such as `node_modules`, `target`, and `.venv`.
- `*` and `?` do not cross directory separators — use `**` to recurse. `{a,b}` alternation is supported.
- Returns at most ${MAX_MATCHES} matches.
