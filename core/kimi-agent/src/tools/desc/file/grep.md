Ripgrep-compatible content search. Walks the working directory (ignore-aware, same rules as Glob) and matches each file against a regex pattern.

Output modes: `files_with_matches` (default) returns one file path per match; `content` returns matching lines; `count_matches` returns per-file match counts.

Use `-F: true` to search for literal text that contains regex metacharacters (e.g. `fn foo(`, `[ERROR]`).
