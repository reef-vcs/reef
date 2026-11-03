"""One RPC's description, as ``reef._rpcs`` lists them."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Generic, Literal, TypeVar

from google.protobuf.message import Message

Req = TypeVar("Req", bound=Message)
Resp = TypeVar("Resp", bound=Message)

Access = Literal["READ", "WRITE", "ADMIN", "UNSPECIFIED"]


@dataclass(frozen=True)
class Rpc(Generic[Req, Resp]):
    """An RPC of ``reef.api.v1``: its path, message types, access level and paging."""

    path: str
    """The gRPC method path, ``/reef.api.v1.<Service>/<Method>``."""
    name: str
    """The method's name in the schema."""
    request: type[Req]
    response: type[Resp]
    access: Access
    """``(reef.api.v1.access)``: a remote daemon serves READ and WRITE only."""
    streaming: bool
    """Whether the daemon streams responses."""
    paged: bool
    """Whether it takes ``page_token`` and answers ``next_page_token``."""
    items: str | None
    """The response's repeated field that ``<method>_all`` yields, for a paged RPC."""
