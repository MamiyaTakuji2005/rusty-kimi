Functional equivalent to python's str.replace(old, new) but with regex matching, proper whitespace handling and a fuzzy matching fallback.

- Multi-line edits supported.
- Use `replace_all: true` to replace all occurrences.
- Set `regex: true` to treat `old` as a regex and reference capture groups with `$1`/`${name}`; write `$$` for a literal `$`, undefined group references are rejected rather than dropped.
- Parameter shape: `{"path": "...", "old": "...", "new": "..."}`.
