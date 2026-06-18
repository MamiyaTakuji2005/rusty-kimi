Read a text file. Each line is returned prefixed with its 1-based line number. Binary and media files are rejected — use ReadMediaFile for images/video.

Use `line_offset` and `n_lines` to read a slice. Max ${MAX_LINES} lines or ${MAX_LINE_LENGTH} chars per line (truncated with `…`).
