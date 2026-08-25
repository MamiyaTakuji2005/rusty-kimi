Replace a string in a file. `old` and `new` may span multiple lines.

- Matching tolerates whitespace differences and falls back to a fuzzy match.
- `replace_all: true` replaces every occurrence; otherwise `old` must appear exactly once.
- `regex: true` reads `old` as a regex — reference groups with `$1` or `${name}`, and write `$$` for a literal `$`. An undefined group reference is rejected rather than dropped.
