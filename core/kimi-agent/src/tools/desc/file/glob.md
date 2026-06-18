Find files and directories using glob patterns. Supports `*`, `?`, `[...]` character classes, `{a,b}` brace alternation, and `**` for recursive search.

The search is ignore-aware: it skips `.gitignore`d paths, hidden dot-entries, and well-known heavy directories (`node_modules`, `target`, `.venv`, `__pycache__`, …), so recursive patterns stay fast and relevant. Results are limited to the first ${MAX_MATCHES} matches.

**When to use:**
- Find files matching a pattern (e.g. all Python files: `*.py`)
- Search recursively (e.g. `**/*.rs`, `src/**/*.js`)
- Locate config files (e.g. `*.config.{js,ts}`, `*.json`)
- Find test files (e.g. `test_*.py`, `*_test.go`)

**Example patterns:**
- `*.py` — Python files in the top level
- `**/*.rs` — all Rust files recursively
- `src/**/*.js` — JavaScript files under `src` recursively
- `*.config.{js,ts}` — config files ending in `.js` or `.ts`

Note: `*` and `?` do not cross directory separators; use `**` to recurse.
