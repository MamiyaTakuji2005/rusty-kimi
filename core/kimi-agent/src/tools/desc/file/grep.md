Full ripgrep-compatible file search. Walks the working directory with the same ignore rules as Glob (gitignore, hidden files, heavy dirs pruned).

- Supports: regex patterns (or `-F: true` for fixed strings), `-i` case-insensitive, `-n` line numbers, `-B`/`-A`/`-C` context lines, `type` file type filter (`rust`, `python`, `js`, `ts`, `go`, `java`, `cpp`, …), `glob` file filter, `multiline` mode, and `head_limit` to cap output.
- Output modes — `files_with_matches` (default): one path per matched file; `content`: matching lines with optional context; `count_matches`: per-file match count.
