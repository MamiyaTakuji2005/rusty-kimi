Execute a ${SHELL} command. Use this tool to run scripts, build/test projects, manage processes, and perform operations that have no dedicated tool. For file reading, writing, searching, and editing use ReadFile, WriteFile, StrReplaceFile, Grep, and Glob instead.

**Output:**
The stdout and stderr streams are combined and returned as a single string. Extremely long output may be truncated. When a command fails, the exit code is provided in a system tag.

**Guidelines for safety and security:**
- Every tool call starts a fresh ${SHELL} session. Environment variables, `cd` changes, and command history do not persist between calls.
- Do not launch interactive programs or anything that is expected to block indefinitely; ensure each command finishes promptly. Provide a `timeout` argument for potentially long runs.
- Avoid using `..` to leave the working directory, and never touch files outside that directory unless explicitly instructed.
- Never attempt commands that require elevated (Administrator) privileges unless explicitly authorized.

**Guidelines for efficiency:**
- Chain related commands with `&&` or `;` and use `if ($?)` / `if (-not $?)` for conditional execution.
- Redirect or pipe output with `>`, `>>`, `|`.
- Use PowerShell cmdlets (`Get-ChildItem`, `Select-String`, `Where-Object`) for filtering rather than separate tool calls.

**Commands available:**
- Shell environment: `Set-Location`, `Get-Location`, `$env:VAR`, `where`
- File system operations: `Get-ChildItem`, `New-Item`, `Copy-Item`, `Move-Item`, `Remove-Item`, `mkdir`
- System info: `Get-Process`, `Stop-Process`, `Get-Service`, `hostname`, `systeminfo`
- Archives/scripts: `Compress-Archive`, `Expand-Archive`, `tar`, `python`, `node`
- Other: Any other binaries available on the system PATH; run `where <command>` first if unsure.
