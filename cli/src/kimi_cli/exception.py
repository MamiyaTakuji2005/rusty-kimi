from __future__ import annotations

from kosong.tooling import ToolError


class KimiCLIException(Exception):
    """Base exception class for Kimi Code CLI."""

    pass


class ConfigError(KimiCLIException, ValueError):
    """Configuration error."""

    pass


class AgentSpecError(KimiCLIException, ValueError):
    """Agent specification error."""

    pass


class InvalidToolError(KimiCLIException, ValueError):
    """Invalid tool error."""

    pass


class SystemPromptTemplateError(KimiCLIException, ValueError):
    """System prompt template error."""

    pass


class MCPConfigError(KimiCLIException, ValueError):
    """MCP config error."""

    pass


class MCPRuntimeError(KimiCLIException, RuntimeError):
    """MCP runtime error."""

    pass


class ToolRejectedError(ToolError):
    """Raised when a tool call is rejected by the user or approval system."""

    has_feedback: bool = False

    def __init__(
        self,
        message: str | None = None,
        brief: str = "Rejected by user",
        has_feedback: bool = False,
    ):
        super().__init__(
            message=message
            or (
                "The tool call is rejected by the user. "
                "Stop what you are doing and wait for the user to tell you how to proceed."
            ),
            brief=brief,
        )
        self.has_feedback = has_feedback
