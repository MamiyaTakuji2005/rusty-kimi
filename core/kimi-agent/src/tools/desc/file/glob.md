Standard glob with two agent safety features

- Ignore-aware walk (respects `.gitignore`, skips hidden files and heavy dirs like `node_modules`, `target`, `.venv`) and a hard cap of ${MAX_MATCHES} results.
- `*` and `?` do not cross directory separators — use `**` to recurse. Brace alternation `{a,b}` is supported.
