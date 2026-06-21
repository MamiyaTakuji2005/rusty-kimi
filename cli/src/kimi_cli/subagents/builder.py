from __future__ import annotations

from kimi_cli.soul.agent import Agent, Runtime
from kimi_cli.subagents.models import AgentLaunchSpec, AgentTypeDefinition


class SubagentBuilder:
    def __init__(self, root_runtime: Runtime):
        self._root_runtime = root_runtime

    async def build_builtin_instance(
        self,
        *,
        agent_id: str,
        type_def: AgentTypeDefinition,
        launch_spec: AgentLaunchSpec,
    ) -> Agent:
        raise RuntimeError(
            "Local subagent LLM execution is disabled; run via the Rust agent (KIMI_AGENT_BIN)."
        )

    @staticmethod
    def resolve_effective_model(
        *, type_def: AgentTypeDefinition, launch_spec: AgentLaunchSpec
    ) -> str | None:
        return launch_spec.model_override or launch_spec.effective_model or type_def.default_model
