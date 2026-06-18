Replace text in a file. Supports exact, fuzzy (whitespace-tolerant), and regex matching. Multi-line edits supported.

Set `regex: true` to treat `old` as a regex. In regex mode, `new` may reference capture groups with `$1`/`${name}`; write `$$` for a literal `$` (an undefined group reference is rejected rather than silently dropped). Use `replace_all: true` to replace all occurrences.
