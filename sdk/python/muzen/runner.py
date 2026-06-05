from __future__ import annotations

import asyncio
import json
from typing import Any, Awaitable, Callable, Dict, Optional

RUNNER_PROTOCOL_VERSION = "muzen.runner.v1"


class RunnerProtocolError(RuntimeError):
    def __init__(self, error: Dict[str, Any]) -> None:
        super().__init__(str(error.get("message", "runner protocol error")))
        self.code = error.get("code")
        data = error.get("data") or {}
        self.kind = data.get("kind")


NotificationListener = Callable[[Dict[str, Any]], None]


class RunnerStdioClient:
    def __init__(self, process: asyncio.subprocess.Process) -> None:
        self._process = process
        self._pending: Dict[int, asyncio.Future[Any]] = {}
        self._listeners: list[NotificationListener] = []
        self._next_request_id = 1
        self._reader_task = asyncio.create_task(self._read_loop())

    @classmethod
    async def start(
        cls,
        runner_path: str,
        runner_args: Optional[list[str]] = None,
    ) -> "RunnerStdioClient":
        process = await asyncio.create_subprocess_exec(
            runner_path,
            *(runner_args or ["stdio"]),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        return cls(process)

    async def handshake(
        self,
        *,
        client_name: str = "muzen-py",
        client_version: Optional[str] = None,
    ) -> Any:
        return await self.request(
            "runner.handshake",
            {
                "protocolVersion": RUNNER_PROTOCOL_VERSION,
                "clientName": client_name,
                "clientVersion": client_version,
            },
        )

    async def request(self, method: str, params: Any = None) -> Any:
        if self._process.stdin is None:
            raise RuntimeError("muzen-runner stdin is closed")
        request_id = self._next_request_id
        self._next_request_id += 1
        loop = asyncio.get_running_loop()
        future: asyncio.Future[Any] = loop.create_future()
        self._pending[request_id] = future
        frame = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }
        self._process.stdin.write((json.dumps(frame) + "\n").encode("utf-8"))
        await self._process.stdin.drain()
        return await future

    def on_notification(self, listener: NotificationListener) -> Callable[[], None]:
        self._listeners.append(listener)

        def unsubscribe() -> None:
            if listener in self._listeners:
                self._listeners.remove(listener)

        return unsubscribe

    async def close(self) -> None:
        if self._process.stdin is not None:
            self._process.stdin.close()
        if self._process.returncode is None:
            self._process.terminate()
            try:
                await asyncio.wait_for(self._process.wait(), timeout=2)
            except asyncio.TimeoutError:
                self._process.kill()
                await self._process.wait()
        self._reader_task.cancel()
        await asyncio.gather(self._reader_task, return_exceptions=True)
        for future in self._pending.values():
            if not future.done():
                future.set_exception(RuntimeError("muzen-runner client closed"))
        self._pending.clear()

    async def _read_loop(self) -> None:
        assert self._process.stdout is not None
        try:
            while True:
                line = await self._process.stdout.readline()
                if not line:
                    break
                self._handle_frame(json.loads(line.decode("utf-8")))
        except asyncio.CancelledError:
            raise
        except Exception as error:
            for future in self._pending.values():
                if not future.done():
                    future.set_exception(error)
            self._pending.clear()

    def _handle_frame(self, frame: Dict[str, Any]) -> None:
        if "method" in frame:
            for listener in list(self._listeners):
                listener(frame)
            return
        request_id = frame.get("id")
        future = self._pending.pop(request_id, None)
        if future is None:
            return
        if frame.get("error"):
            future.set_exception(RunnerProtocolError(frame["error"]))
        else:
            future.set_result(frame.get("result"))
