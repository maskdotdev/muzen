from __future__ import annotations

from typing import List, Optional

from .errors import MuzenUnsupportedFeatureError
from .local import Client
from .remote import RemoteClient, RemoteTransport
from .runner_mapping import _to_runner_start_params, _to_swarm_start_params


async def create_muzen(
    *,
    runner_path: Optional[str] = None,
    runner_args: Optional[List[str]] = None,
    client_name: str = "muzen-py",
    client_version: Optional[str] = None,
) -> Client:
    return await Client.create(
        runner_path=runner_path,
        runner_args=runner_args,
        client_name=client_name,
        client_version=client_version,
    )


def create_muzen_client(
    *,
    base_url: str,
    token: Optional[str] = None,
    transport: Optional[RemoteTransport] = None,
) -> RemoteClient:
    return RemoteClient(base_url=base_url, token=token, transport=transport)


__all__ = [
    "Client",
    "MuzenUnsupportedFeatureError",
    "RemoteClient",
    "RemoteTransport",
    "_to_runner_start_params",
    "_to_swarm_start_params",
    "create_muzen",
    "create_muzen_client",
]
