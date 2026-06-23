"""Vestigial local-execution soul.

The local agent loop has been removed — execution now lives entirely in the
Rust agent (``KIMI_AGENT_BIN``). Only two things are still referenced by the
live shim:

- ``KimiSoul`` as a type for ``isinstance(soul, KimiSoul)`` checks in the
  shell. In remote mode the active soul is always ``RemoteSoul``, so those
  checks are always ``False``; the class exists purely so those checks resolve.
- the slash-command prefix constants.

Constructing a ``KimiSoul`` raises: there is no local execution to drive.
"""

from __future__ import annotations

SKILL_COMMAND_PREFIX = "skill:"
FLOW_COMMAND_PREFIX = "flow:"


class KimiSoul:
    """Placeholder for the removed local soul. Never instantiated in remote mode."""

    def __init__(self, *args: object, **kwargs: object) -> None:
        raise RuntimeError(
            "Local LLM execution is disabled; run via the Rust agent (KIMI_AGENT_BIN)."
        )
