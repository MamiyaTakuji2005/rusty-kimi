# rusty-kimi

A unified fork that runs the **Kimi Code CLI shell UI on a Rust agent core**,
connected over the Wire (JSON-RPC over stdio) protocol.

This repository vendors the **full source** of two Apache-2.0 projects from
Moonshot AI (both since discontinued upstream) so they can be developed,
referenced, and shipped together:

| Path    | What it is | Upstream |
|---------|------------|----------|
| `core/` | The Rust agent core — agent loop, tools, MCP, Wire server | [MoonshotAI/kimi-agent-rs](https://github.com/MoonshotAI/kimi-agent-rs) |
| `cli/`  | The Python shell UI + the Wire bridge that drives an external agent | [MoonshotAI/kimi-cli](https://github.com/MoonshotAI/kimi-cli) |

## How they fit together

The Python shell never talked to a backend directly — each turn streams over an
in-process `Wire`. The bridge (`cli/src/kimi_cli/wire/client.py` +
`soul/remote.py`) lets the **same shell UI** drive an *external* Wire agent
(`core/`'s `kimi-agent`) over stdio instead of an in-process Python soul. The
agent owns the loop, tools, LLM and session; the UI is reused unchanged.

```
shell UI (cli/, Python)  ──Wire JSON-RPC / stdio──▶  kimi-agent (core/, Rust)
```

## Quick start (shell UI on the Rust core)

```sh
# build the Rust core
cd core && cargo build -p kimi-agent && cd ..

# run the Python shell against it
cd cli
PYTHONPATH=src KIMI_AGENT_BIN="../core/target/debug/kimi-agent" python -m kimi_cli
```

On Windows (PowerShell), quote the binary path and set `PYTHONUTF8=1`. The
welcome banner shows `(remote / wire)` when it's running on the Rust core.

Without `KIMI_AGENT_BIN`, `cli/` runs its original in-process Python agent.

## What works on the Rust core today

Serve OpenAI-compatible / `openai_legacy` providers (deepseek, openrouter, glm,
…), `prompt`, `cancel`, `replay` (resume re-render), `steer` (mid-turn
injection), tools with approvals, model selection, YOLO. `set_plan_mode` is
not yet implemented in the Rust Wire server.

## License & attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). Both
subtrees are modified forks of Moonshot AI projects; per-subtree git history is
preserved. This is an independent fork, not affiliated with or endorsed by
Moonshot AI; "Kimi"/"Moonshot" are trademarks of their owners.
