Read a text file. Each line is returned prefixed with its 1-based line number.

- Reads at most ${MAX_LINES} lines, and truncates any line longer than ${MAX_LINE_LENGTH} characters.
- `line_offset` picks the first line to return; a negative value counts back from the end of the file.
- Binary files are rejected, and images and video belong to ReadMediaFile.
