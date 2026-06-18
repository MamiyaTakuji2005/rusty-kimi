Read a text file. Output is `cat -n` format (1-based line numbers). Binary and media files are rejected — use ReadMediaFile for images/video.

Use `line_offset` and `n_lines` to read a slice. Max ${MAX_LINES} lines or ${MAX_LINE_LENGTH} chars per line (truncated with `…`).
