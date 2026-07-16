from __future__ import annotations

import asyncio
import inspect
import json
import re
import threading
import types
from dataclasses import MISSING, dataclass, fields, is_dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import (
    Any,
    Callable,
    Dict,
    List,
    Optional,
    Set,
    Type,
    Union,
    get_args,
    get_origin,
    get_type_hints,
)

from .agent import MuzenError


_TOOL_NAME = re.compile(r"^[a-zA-Z0-9_-]{1,64}$")


@dataclass(frozen=True)
class Tool:
    """A Python function exposed to an Agent as a local MCP tool."""

    function: Callable[..., Any]
    name: str
    description: str
    input_schema: Dict[str, Any]

    def invoke(self, arguments: Dict[str, Any]) -> Any:
        value = self.function(**arguments)
        if inspect.isawaitable(value):
            return asyncio.run(value)
        return value


def tool(function: Callable[..., Any]) -> Tool:
    """Wrap a typed sync or async function as a Muzen tool."""
    if isinstance(function, Tool):
        return function
    if not inspect.isfunction(function):
        raise _invalid("tool", "must be a function")
    name = function.__name__
    if _TOOL_NAME.fullmatch(name) is None:
        raise _invalid(
            "tool.name", "must match [a-zA-Z0-9_-]{1,64}"
        )
    return Tool(
        function=function,
        name=name,
        description=_first_docstring_paragraph(function),
        input_schema=_function_schema(function),
    )


class LoopbackToolServer:
    """Small Streamable HTTP MCP server for one Agent's Python tools."""

    def __init__(self, tools: tuple[Tool, ...]) -> None:
        self._tools = {item.name: item for item in tools}
        self._server: Optional[ThreadingHTTPServer] = None
        self._thread: Optional[threading.Thread] = None

    @property
    def url(self) -> str:
        if self._server is None:
            raise RuntimeError("tool server has not been started")
        return "http://127.0.0.1:%d/mcp" % self._server.server_port

    def start(self) -> None:
        if self._server is not None:
            return
        tools = self._tools

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                if self.path != "/mcp":
                    self.send_error(404)
                    return
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    request = json.loads(self.rfile.read(length).decode("utf-8"))
                except (ValueError, UnicodeDecodeError, json.JSONDecodeError):
                    self._json_error(None, -32700, "invalid JSON")
                    return
                request_id = request.get("id")
                method = request.get("method")
                if method == "notifications/initialized":
                    self.send_response(202)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if method == "initialize":
                    self._json_result(
                        request_id,
                        {
                            "protocolVersion": "2025-03-26",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "muzen-python-tools", "version": "1"},
                        },
                        session_id="muzen-python-tools",
                    )
                    return
                if method == "tools/list":
                    listed = [
                        {
                            "name": item.name,
                            "description": item.description,
                            "inputSchema": item.input_schema,
                        }
                        for item in tools.values()
                    ]
                    self._json_result(request_id, {"tools": listed})
                    return
                if method == "tools/call":
                    params = request.get("params") or {}
                    selected = tools.get(params.get("name"))
                    if selected is None:
                        self._json_error(request_id, -32602, "unknown tool")
                        return
                    arguments = params.get("arguments", {})
                    if not isinstance(arguments, dict):
                        self._json_error(request_id, -32602, "arguments must be an object")
                        return
                    try:
                        value = selected.invoke(arguments)
                        encoded = json.dumps(value)
                        text = value if isinstance(value, str) else encoded
                        result: Dict[str, Any] = {
                            "content": [{"type": "text", "text": text}],
                            "isError": False,
                        }
                        if not isinstance(value, str):
                            result["structuredContent"] = value
                    except Exception as exc:
                        result = {
                            "content": [{"type": "text", "text": str(exc)}],
                            "isError": True,
                        }
                    self._json_result(request_id, result)
                    return
                self._json_error(request_id, -32601, "method not found")

            def _json_result(
                self, request_id: Any, result: Dict[str, Any], session_id: Optional[str] = None
            ) -> None:
                self._write_json(
                    {"jsonrpc": "2.0", "id": request_id, "result": result},
                    session_id,
                )

            def _json_error(self, request_id: Any, code: int, message: str) -> None:
                self._write_json(
                    {
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {"code": code, "message": message},
                    }
                )

            def _write_json(
                self, value: Dict[str, Any], session_id: Optional[str] = None
            ) -> None:
                body = json.dumps(value).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                if session_id is not None:
                    self.send_header("Mcp-Session-Id", session_id)
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: Any) -> None:
                pass

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(
            target=self._server.serve_forever,
            name="muzen-python-tools",
            daemon=True,
        )
        self._thread.start()

    def close(self) -> None:
        if self._server is None:
            return
        self._server.shutdown()
        self._server.server_close()
        if self._thread is not None:
            self._thread.join(timeout=2)
        self._server = None
        self._thread = None


def annotation_schema(annotation: Any, active: Set[Type[Any]], path: str) -> Dict[str, Any]:
    primitive = {str: "string", int: "integer", float: "number", bool: "boolean"}
    if annotation in primitive:
        return {"type": primitive[annotation]}
    if isinstance(annotation, type) and (_is_typed_dict(annotation) or is_dataclass(annotation)):
        return _object_schema(annotation, active, path)
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin in (list, List):
        if len(args) != 1:
            raise _invalid(path, "list annotations must have one item type")
        return {"type": "array", "items": annotation_schema(args[0], active, path)}
    union_origins = [Union]
    union_type = getattr(types, "UnionType", None)
    if union_type is not None:
        union_origins.append(union_type)
    if origin in tuple(union_origins) and len(args) == 2 and type(None) in args:
        item = args[0] if args[1] is type(None) else args[1]
        return {"anyOf": [annotation_schema(item, active, path), {"type": "null"}]}
    raise _invalid(path, "contains unsupported annotation %r" % (annotation,))


def _function_schema(function: Callable[..., Any]) -> Dict[str, Any]:
    try:
        hints = get_type_hints(function)
    except (NameError, TypeError) as exc:
        raise _invalid("tool.%s" % function.__name__, "contains an unresolved annotation: %s" % exc)
    properties: Dict[str, Any] = {}
    required = []
    for parameter in inspect.signature(function).parameters.values():
        path = "tool.%s.%s" % (function.__name__, parameter.name)
        if parameter.kind in (parameter.VAR_POSITIONAL, parameter.VAR_KEYWORD):
            raise _invalid(path, "variadic parameters are unsupported")
        if parameter.kind == parameter.POSITIONAL_ONLY:
            raise _invalid(path, "positional-only parameters are unsupported")
        if parameter.name not in hints:
            raise _invalid(path, "must have a supported type annotation")
        properties[parameter.name] = annotation_schema(hints[parameter.name], set(), path)
        if parameter.default is inspect.Parameter.empty:
            required.append(parameter.name)
    schema: Dict[str, Any] = {
        "type": "object",
        "properties": properties,
        "additionalProperties": False,
    }
    if required:
        schema["required"] = required
    return schema


def _object_schema(value: Type[Any], active: Set[Type[Any]], path: str) -> Dict[str, Any]:
    if value in active:
        raise _invalid(path, "recursive annotations are unsupported")
    active.add(value)
    try:
        hints = get_type_hints(value)
        properties = {
            name: annotation_schema(annotation, active, path)
            for name, annotation in hints.items()
        }
        if _is_typed_dict(value):
            required_keys = getattr(value, "__required_keys__", None)
            required = list(hints) if required_keys is None else [
                name for name in hints if name in required_keys
            ]
        else:
            field_by_name = {item.name: item for item in fields(value)}
            required = [
                name for name in hints
                if field_by_name[name].default is MISSING
                and field_by_name[name].default_factory is MISSING
            ]
    except (NameError, TypeError) as exc:
        raise _invalid(path, "contains an unresolved annotation: %s" % exc)
    finally:
        active.remove(value)
    schema: Dict[str, Any] = {
        "type": "object",
        "properties": properties,
        "additionalProperties": False,
    }
    if required:
        schema["required"] = required
    return schema


def _first_docstring_paragraph(function: Callable[..., Any]) -> str:
    doc = inspect.getdoc(function) or ""
    paragraph = doc.split("\n\n", 1)[0]
    return " ".join(line.strip() for line in paragraph.splitlines())


def _is_typed_dict(value: Any) -> bool:
    return (
        isinstance(value, type)
        and issubclass(value, dict)
        and hasattr(value, "__annotations__")
        and hasattr(value, "__total__")
    )


def _invalid(path: str, message: str) -> MuzenError:
    return MuzenError("invalid_input", "%s %s" % (path, message), False, {"path": path})
