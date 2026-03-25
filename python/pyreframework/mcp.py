"""MCP (Model Context Protocol) server support for Pyre.

Implements JSON-RPC 2.0 over HTTP at the /mcp endpoint.
AI applications (Claude Desktop, etc.) can discover and invoke tools,
read resources, and use prompt templates.

Usage::

    from pyreframework import Pyre

    app = Pyre()

    @app.mcp.tool(description="Add two numbers")
    def add(a: int, b: int) -> int:
        return a + b

    @app.mcp.resource("config://app")
    def get_config():
        return {"version": "1.0", "debug": False}

    @app.mcp.prompt("greeting", description="Generate a greeting")
    def greeting(name: str) -> str:
        return f"Hello {name}, how can I help you today?"

    app.run()
"""

from __future__ import annotations

import inspect
import json
from typing import Any, Callable, Optional


def _extract_schema(fn: Callable) -> dict:
    """Auto-generate JSON schema from function signature."""
    sig = inspect.signature(fn)
    hints = fn.__annotations__ if hasattr(fn, "__annotations__") else {}
    properties = {}
    required = []

    type_map = {
        int: "integer",
        float: "number",
        str: "string",
        bool: "boolean",
        list: "array",
        dict: "object",
    }

    for name, param in sig.parameters.items():
        if name in ("self", "cls"):
            continue
        hint = hints.get(name)
        prop = {"type": type_map.get(hint, "string")}
        properties[name] = prop
        if param.default is inspect.Parameter.empty:
            required.append(name)

    schema = {"type": "object", "properties": properties}
    if required:
        schema["required"] = required
    return schema


class MCPServer:
    """MCP protocol handler — registers tools, resources, and prompts."""

    def __init__(self) -> None:
        self._tools: dict[str, dict] = {}
        self._resources: dict[str, dict] = {}
        self._prompts: dict[str, dict] = {}

    # ------------------------------------------------------------------
    # Decorators
    # ------------------------------------------------------------------

    def tool(
        self,
        fn: Callable | None = None,
        *,
        name: str | None = None,
        description: str | None = None,
        input_schema: dict | None = None,
    ):
        """Register a tool that AI models can invoke."""

        def register(f: Callable) -> Callable:
            tool_name = name or f.__name__
            self._tools[tool_name] = {
                "name": tool_name,
                "description": description or f.__doc__ or "",
                "inputSchema": input_schema or _extract_schema(f),
                "handler": f,
            }
            return f

        if fn is not None:
            return register(fn)
        return register

    def resource(
        self,
        uri: str,
        *,
        name: str | None = None,
        description: str | None = None,
        mime_type: str = "application/json",
    ):
        """Register a readable resource with URI template support."""

        def register(fn: Callable) -> Callable:
            self._resources[uri] = {
                "uri": uri,
                "name": name or fn.__name__,
                "description": description or fn.__doc__ or "",
                "mimeType": mime_type,
                "handler": fn,
            }
            return fn

        return register

    def prompt(
        self,
        name: str,
        *,
        description: str | None = None,
        arguments: list[dict] | None = None,
    ):
        """Register a prompt template."""

        def register(fn: Callable) -> Callable:
            self._prompts[name] = {
                "name": name,
                "description": description or fn.__doc__ or "",
                "arguments": arguments or [
                    {"name": p, "required": param.default is inspect.Parameter.empty}
                    for p, param in inspect.signature(fn).parameters.items()
                ],
                "handler": fn,
            }
            return fn

        return register

    # ------------------------------------------------------------------
    # JSON-RPC 2.0 handler
    # ------------------------------------------------------------------

    def handle_request(self, body: str) -> str:
        """Process a JSON-RPC 2.0 request and return a response."""
        try:
            req = json.loads(body)
        except json.JSONDecodeError:
            return self._error_response(None, -32700, "Parse error")

        req_id = req.get("id")
        method = req.get("method", "")
        params = req.get("params", {})

        handler_map = {
            "initialize": self._handle_initialize,
            "tools/list": self._handle_tools_list,
            "tools/call": self._handle_tools_call,
            "resources/list": self._handle_resources_list,
            "resources/read": self._handle_resources_read,
            "prompts/list": self._handle_prompts_list,
            "prompts/get": self._handle_prompts_get,
        }

        handler = handler_map.get(method)
        if handler is None:
            return self._error_response(req_id, -32601, f"Method not found: {method}")

        try:
            result = handler(params)
            return json.dumps({"jsonrpc": "2.0", "id": req_id, "result": result})
        except Exception as e:
            return self._error_response(req_id, -32000, str(e))

    # ------------------------------------------------------------------
    # Method handlers
    # ------------------------------------------------------------------

    def _handle_initialize(self, params: dict) -> dict:
        return {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {"listChanged": False},
                "resources": {"subscribe": False, "listChanged": False},
                "prompts": {"listChanged": False},
            },
            "serverInfo": {"name": "pyre-mcp", "version": __import__("pyreframework").__version__},
        }

    def _handle_tools_list(self, params: dict) -> dict:
        tools = [
            {
                "name": t["name"],
                "description": t["description"],
                "inputSchema": t["inputSchema"],
            }
            for t in self._tools.values()
        ]
        return {"tools": tools}

    def _handle_tools_call(self, params: dict) -> dict:
        tool_name = params.get("name", "")
        tool = self._tools.get(tool_name)
        if tool is None:
            raise ValueError(f"Unknown tool: {tool_name}")

        arguments = params.get("arguments", {})
        result = tool["handler"](**arguments)

        # Convert result to MCP content format
        if isinstance(result, str):
            content = [{"type": "text", "text": result}]
        elif isinstance(result, dict):
            content = [{"type": "text", "text": json.dumps(result)}]
        else:
            content = [{"type": "text", "text": str(result)}]

        return {"content": content, "isError": False}

    def _handle_resources_list(self, params: dict) -> dict:
        resources = [
            {
                "uri": r["uri"],
                "name": r["name"],
                "description": r["description"],
                "mimeType": r["mimeType"],
            }
            for r in self._resources.values()
        ]
        return {"resources": resources}

    def _handle_resources_read(self, params: dict) -> dict:
        uri = params.get("uri", "")
        resource = self._resources.get(uri)
        if resource is None:
            raise ValueError(f"Unknown resource: {uri}")

        result = resource["handler"]()
        if isinstance(result, str):
            text = result
        else:
            text = json.dumps(result)

        return {
            "contents": [
                {"uri": uri, "mimeType": resource["mimeType"], "text": text}
            ]
        }

    def _handle_prompts_list(self, params: dict) -> dict:
        prompts = [
            {
                "name": p["name"],
                "description": p["description"],
                "arguments": p["arguments"],
            }
            for p in self._prompts.values()
        ]
        return {"prompts": prompts}

    def _handle_prompts_get(self, params: dict) -> dict:
        prompt_name = params.get("name", "")
        prompt = self._prompts.get(prompt_name)
        if prompt is None:
            raise ValueError(f"Unknown prompt: {prompt_name}")

        arguments = params.get("arguments", {})
        result = prompt["handler"](**arguments)

        return {
            "description": prompt["description"],
            "messages": [
                {"role": "user", "content": {"type": "text", "text": str(result)}}
            ],
        }

    @staticmethod
    def _error_response(req_id: Any, code: int, message: str) -> str:
        return json.dumps({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": code, "message": message},
        })
