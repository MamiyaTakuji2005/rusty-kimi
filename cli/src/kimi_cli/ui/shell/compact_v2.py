from __future__ import annotations

import json
import re
from typing import TYPE_CHECKING

from kosong.message import Message, TextPart, ThinkPart, ToolCall

from kimi_cli.ui.shell.console import console
from kimi_cli.ui.shell.slash import registry

if TYPE_CHECKING:
    from kimi_cli.ui.shell import Shell


_AUTO_DROP_ROLES = {"_checkpoint", "_usage", "_system_prompt"}
_FETCH_MARKER = "<system>The returned content is the main content"

_DEFAULT_KEEP_PCT = 30


def _text_from_content(content: object) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for p in content:
            if isinstance(p, TextPart):
                parts.append(p.text)
            elif isinstance(p, dict):
                parts.append(str(p.get("text", "")))
        return " ".join(parts)
    return str(content)


def _should_auto_drop(msg: Message) -> bool:
    if msg.role in _AUTO_DROP_ROLES:
        return True
    if msg.role == "tool":
        text = _text_from_content(msg.content)
        if "Title:" in text[:100] and "URL:" in text[:200]:
            return True
        if _FETCH_MARKER in text[:200]:
            return True
    if msg.role == "assistant" and not msg.tool_calls:
        if len(_text_from_content(msg.content)) > 200:
            return True
    return False


def _entry_preview(msg: Message, idx: int) -> str:
    role = msg.role

    if role == "user":
        text = _text_from_content(msg.content)[:200].replace("\n", " ")
        return f"E{idx:04d} user | {text}"

    if role == "assistant":
        tcs: list[ToolCall] = msg.tool_calls or []
        tool_names = [tc.function.name for tc in tcs]
        paths: list[str] = []
        for tc in tcs:
            fn = tc.function.name
            try:
                args = json.loads(tc.function.arguments or "{}")
                if "path" in args:
                    paths.append(str(args["path"]).replace("\\", "/").rsplit("/", 1)[-1])
                if fn == "Shell" and "command" in args:
                    paths.append(f"cmd:{str(args['command'])[:60].replace(chr(10), ' ')}")
                if fn == "Agent" and "model" in args:
                    paths.append(f"model:{args['model']}")
            except Exception:
                pass
        path_str = f" ({', '.join(paths[:3])})" if paths else ""
        return f"E{idx:04d} assistant | {','.join(tool_names[:4])}{path_str}"

    if role == "tool":
        text = _text_from_content(msg.content)
        lower = text.lower()
        if "error" in lower[:100]:
            tag, snippet = " [ERROR]", text[:150]
        elif "agent_id:" in lower[:60]:
            tag, snippet = " [AGENT_RESULT]", text[:100]
        elif "command executed" in lower[:100]:
            tag, snippet = " [SHELL]", text[:100]
        else:
            tag, snippet = "", text[:100]
        return f"E{idx:04d} tool{tag} | {snippet.replace(chr(10), ' ')}"

    return f"E{idx:04d} {role}"


_CLASSIFY_PROMPT = """\
Rank the entries from a coding agent session by importance for continuity.
Return a JSON array of INTEGER IDs (the number after E in each entry label), ordered MOST to LEAST important.
Example output: [20, 0, 21, 158, 55]  — integers only, no quotes, no E-prefix.
Include every entry that has any significance — omit only pure noise.
Do NOT apply any budget or cutoff. Just rank what matters.

HIGH importance:
- User messages that set tasks, change direction, or ask questions
- Assistant tool_calls that wrote files, ran commands, made architectural decisions
- Tool outputs showing errors or final results of significant operations

LOW importance (include but rank last):
- Short user acknowledgements that moved the session forward
- Tool outputs showing intermediate/successful steps with no surprises

OMIT entirely:
- User messages that are just "ok", "yes", "continue", "sure"
- Assistant entries without tool_calls (prose only)
- Search result dumps, web page fetches, large read-only file outputs
- Repeated similar calls where only the last result matters

ENTRIES:"""

_SUMMARIZE_PROMPT = """\
Write a brief intro paragraph (3-5 sentences) for a coding session that has been compacted.
This will be injected at the start of the kept context so the agent knows where it is.
Cover: what the session is about, what was already accomplished, and the current state.
Write it as if briefing someone picking up mid-session — not a list, a natural orientation.
No preamble, just the intro.

DROPPED ENTRIES (one-line previews of what was removed):
{previews}

Return only the intro paragraph."""


def _coerce_id(v: object) -> int | None:
    """Convert an entry ID to an integer, handling both 20 and 'E0020' forms."""
    if isinstance(v, int):
        return v
    if isinstance(v, str):
        s = v.strip().lstrip("Ee")
        try:
            return int(s)
        except ValueError:
            pass
    return None


def _parse_ranked_ids(result: str, n_entries: int) -> list[int]:
    """Parse LLM response into a list of valid integer entry IDs."""
    result = result.strip()

    def _valid(ids: list) -> list[int]:
        out = []
        seen: set[int] = set()
        for v in ids:
            k = _coerce_id(v)
            if k is not None and 0 <= k < n_entries and k not in seen:
                out.append(k)
                seen.add(k)
        return out

    # Strip markdown code fences
    stripped = re.sub(r"```[a-z]*\n?", "", result).strip()

    for text in (result, stripped):
        try:
            parsed = json.loads(text)
            if isinstance(parsed, list):
                ids = _valid(parsed)
                if ids:
                    return ids
        except json.JSONDecodeError:
            pass

    # Find any [...] block (handles prose + array, E-prefix strings or integers)
    m = re.search(r"\[[\w\s,\"']+\]", result, re.DOTALL)
    if m:
        try:
            parsed = json.loads(m.group())
            if isinstance(parsed, list):
                ids = _valid(parsed)
                if ids:
                    return ids
        except json.JSONDecodeError:
            pass

    # Last resort: pull every E-prefixed label or bare number
    candidates = [int(x) for x in re.findall(r"\bE(\d+)\b", result)]
    if not candidates:
        candidates = [int(x) for x in re.findall(r"\b(\d+)\b", result)]
    return [k for k in dict.fromkeys(candidates) if 0 <= k < n_entries]


async def _llm_call(prompt: str, provider_name: str, model_name: str, config) -> str:
    from openai import AsyncOpenAI

    provider = config.providers[provider_name]
    client = AsyncOpenAI(
        api_key=provider.api_key.get_secret_value(),
        base_url=provider.base_url,
    )
    response = await client.chat.completions.create(
        model=model_name,
        messages=[{"role": "user", "content": prompt}],
        max_tokens=8192,
    )
    msg = response.choices[0].message
    content = msg.content or ""
    if not content.strip():
        # Thinking models (DeepSeek, etc.) put the answer in reasoning_content
        # when temperature is not set or content comes back empty
        content = getattr(msg, "reasoning_content", None) or ""
    return content


def _pick_provider(config, hint: str | None) -> tuple[str, str] | None:
    providers = config.providers
    models = config.models

    def _first_model_for(pname: str) -> str | None:
        for m in models.values():
            if m.provider == pname:
                return m.model
        return None

    if hint:
        if hint not in providers:
            return None
        m = _first_model_for(hint)
        return (hint, m) if m else None

    for pname in ("deepseek", "glm", "openrouter"):
        if pname in providers:
            m = _first_model_for(pname)
            if m:
                return (pname, m)

    from kimi_cli.llm import ProviderType
    for pname, p in providers.items():
        if p.type in (ProviderType.OpenaiLegacy, ProviderType.OpenAiCompatible):
            m = _first_model_for(pname)
            if m:
                return (pname, m)
    return None


def _parse_args(args: str) -> dict:
    arg_list = args.split()
    result: dict = {
        "summarize": "--summarize" in arg_list or "-s" in arg_list,
        "dry_run": "--dry-run" in arg_list,
        "provider": None,
        "keep_pct": _DEFAULT_KEEP_PCT,
    }
    for flag in ("--provider", "--keep"):
        if flag in arg_list:
            i = arg_list.index(flag)
            if i + 1 < len(arg_list):
                val = arg_list[i + 1]
                if flag == "--provider":
                    result["provider"] = val
                elif flag == "--keep":
                    try:
                        result["keep_pct"] = int(val.rstrip("%"))
                    except ValueError:
                        pass
    return result


@registry.command(name="compact-v2")
async def compact_v2(app: Shell, args: str) -> None:
    """LLM-classifier compaction: ranks entries by importance, keeps top N%"""
    if app.runtime is None:
        console.print("[red]No runtime available.[/red]")
        return

    opts = _parse_args(args)
    keep_pct: int = max(5, min(90, opts["keep_pct"]))

    config = app.runtime.config
    pick = _pick_provider(config, opts["provider"])
    if pick is None:
        console.print("[red]No suitable provider found. Specify one with --provider <name>.[/red]")
        return
    provider_name, model_name = pick

    # Load context from disk (works in both local and remote mode)
    from kimi_cli.soul.context import Context

    context = Context(app.runtime.session.context_file)
    await context.restore()
    history = list(context.history)

    if not history:
        console.print("[yellow]Context is empty — nothing to compact.[/yellow]")
        return

    kept_for_llm: list[tuple[int, Message]] = []
    auto_dropped = 0
    for i, msg in enumerate(history):
        if _should_auto_drop(msg):
            auto_dropped += 1
        else:
            kept_for_llm.append((i, msg))

    console.print(
        f"[cyan]Messages: {len(history)} total, {auto_dropped} auto-dropped, "
        f"{len(kept_for_llm)} sent to classifier[/cyan]"
    )
    console.print(f"[cyan]Provider: {provider_name}/{model_name}[/cyan]")

    classify_prompt = (
        _CLASSIFY_PROMPT
        + "\n"
        + "\n".join(_entry_preview(msg, i) for i, (_, msg) in enumerate(kept_for_llm))
    )

    if opts["dry_run"]:
        console.print("[yellow][DRY RUN] Classifier prompt (first 40 lines):[/yellow]")
        for line in classify_prompt.split("\n")[:40]:
            console.print(line)
        console.print(f"[yellow]Total lines: {len(classify_prompt.splitlines())}[/yellow]")
        return

    with console.status("[cyan]Classifying...[/cyan]"):
        try:
            result = await _llm_call(classify_prompt, provider_name, model_name, config)
        except Exception as exc:
            console.print(f"[red]LLM call failed: {exc}[/red]")
            return

    ranked_ids = _parse_ranked_ids(result, len(kept_for_llm))

    if not ranked_ids:
        console.print(
            f"[red]Classifier returned 0 valid IDs — aborting to avoid wiping context.[/red]\n"
            f"[yellow]Raw response (first 400 chars):[/yellow]\n{result[:400]}"
        )
        return

    cutoff = max(1, len(ranked_ids) * keep_pct // 100)
    kept_local_ids = ranked_ids[:cutoff]
    kept_local_set = set(kept_local_ids)

    console.print(
        f"[cyan]Classifier ranked {len(ranked_ids)} entries — "
        f"keeping top {cutoff} ({keep_pct}% of ranked)[/cyan]"
    )

    kept_orig_indices = sorted(kept_for_llm[i][0] for i in kept_local_set)
    dropped_for_summary: list[tuple[int, Message]] = [
        (local_idx, msg)
        for local_idx, (_, msg) in enumerate(kept_for_llm)
        if local_idx not in kept_local_set
    ]
    kept_orig = [history[i] for i in kept_orig_indices]

    pct_actual = 100 * len(kept_orig) // len(history) if history else 0
    console.print(
        f"[green]Kept {len(kept_orig)}/{len(history)} messages ({pct_actual}% of original)[/green]"
    )

    summary_text: str | None = None
    if opts["summarize"] and dropped_for_summary:
        previews = "\n".join(
            _entry_preview(msg, local_idx) for local_idx, msg in dropped_for_summary
        )
        with console.status("[cyan]Writing intro...[/cyan]"):
            try:
                summary_text = (
                    await _llm_call(
                        _SUMMARIZE_PROMPT.format(previews=previews),
                        provider_name,
                        model_name,
                        config,
                    )
                ).strip()
            except Exception as exc:
                console.print(f"[yellow]Intro failed (skipping): {exc}[/yellow]")

    system_prompt = context.system_prompt

    # Fork into a new session so wire.jsonl also starts clean.
    # Reloading the same session would replay old wire.jsonl,
    # making the display show the old token count despite the compacted context.
    from kimi_cli.cli import Reload
    from kimi_cli.session import Session
    from kimi_cli.session_state import load_session_state, save_session_state

    session = app.runtime.session
    new_session = await Session.create(work_dir=session.work_dir)

    # Write compacted context to the new session
    new_context = Context(new_session.context_file)
    if system_prompt is not None:
        await new_context.write_system_prompt(system_prompt)

    if summary_text:
        from kimi_cli.soul.message import system as system_tag

        await new_context.append_message(
            Message(role="user", content=[system_tag(summary_text)])
        )
        console.print("[green]Intro injected.[/green]")

    if kept_orig:
        await new_context.append_message(kept_orig)

    # Label the new session
    src_state = load_session_state(session.dir)
    new_state = load_session_state(new_session.dir)
    new_state.custom_title = f"Compact: {src_state.custom_title or 'Untitled'}"
    new_state.title_generated = True
    save_session_state(new_state, new_session.dir)

    console.print(f"[green]Done. Switching to compacted session...[/green]")
    raise Reload(session_id=new_session.id)
