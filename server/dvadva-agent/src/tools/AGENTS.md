# Tools Module Guide

This directory holds the built-in tools exposed to the agent runtime.

- `shell.rs`: `Shell` tool — runs commands with approval gating and output truncation.
- `file/`: File tool implementations and shared helpers.
  - `read.rs`: `ReadFile` tool (text reading with size/line limits).
  - `read_media.rs`: `ReadMediaFile` tool (image/video loading + Kimi upload path).
  - `write.rs`: `WriteFile` tool (diff preview + approval gating).
  - `replace.rs`: `StrReplaceFile` tool (string edits + diff/approval).
  - `glob.rs`: `Glob` tool.
  - `grep.rs`: `Grep` tool (rg-backed search).
  - `mod.rs`: shared file detection helpers/constants + re-exports.
- `web/`: Web tool implementations.
  - `search.rs`: `SearchWeb` tool (Moonshot search service).
  - `fetch.rs`: `FetchURL` tool (Moonshot fetch service + HTML extraction).
- `agent.rs`: `Agent` tool — spawns a subagent subprocess with a fresh context.
- `fork.rs`: `Fork` tool — spawns a subagent seeded with the parent's context.
- `task/`: Background task tools (`TaskList`, `TaskOutput`, `TaskStop`).
- `snapshot.rs`: `Undo` tool, backed by the `CachedKaos` write history.
- `todo.rs`: `SetTodoList` tool emitting `TodoDisplayBlock` updates.
- `dmail.rs`: `SendDMail` tool wired to `DenwaRenji`.
- `think.rs`: `Think` tool — thought logging.
- `test.rs`: Test-only math/panic tools (`plus`, `compare`, `panic`).
- `desc/`: Markdown tool descriptions, one file per tool, mirroring the module
  layout. `utils::load_desc` substitutes `${TOKEN}` placeholders at load time.
- `utils.rs`: Shared helpers (result builder, truncation, description templating).

## Writing a tool description

Every `desc/*.md` file follows the same shape, so the tool list reads as one
document rather than a dozen voices:

```markdown
<One imperative sentence saying what the tool does.>

- <A constraint, limit, or behaviour the model would otherwise guess wrong.>
- <...>
```

Rules:

- Lead with a single sentence. No bold headers, no restating the tool's name.
- Then flat `-` bullets. No nested bullets, no `**Section:**` headings.
- Say only what a competent model cannot infer: limits, non-default behaviour,
  which sibling tool to use instead. Never explain what `Grep` or `WriteFile`
  are for.
- Standard tools (read, write, edit, search, shell, web) stay at or under three
  bullets. Tools with no equivalent elsewhere — `SendDMail`, `Fork`, `Agent`,
  `Undo`, the task tools — may add a short paragraph and up to four.
- Refer to sibling tools by their exact registered name.
