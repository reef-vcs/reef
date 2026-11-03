"""The asyncio client (R2.06, ADR 0054): the sync client's surface, awaited."""

from __future__ import annotations

import asyncio
import logging
import os
from types import TracebackType
from typing import TYPE_CHECKING, Any, TypeVar

import grpc
from google.protobuf.message import Message

from reef import _channel, _connect, _identity
from reef._rpc import Rpc
from reef.aio._methods import Methods
from reef.aio._stream import AsyncStream
from reef.errors import (
    InvalidArgumentError,
    ReefError,
    from_rpc_error,
)

if TYPE_CHECKING:
    from reef.aio.workspace import AsyncWorkspaceBlock
    from reef.api.v1 import GetDaemonStatusResponse

Req = TypeVar("Req", bound=Message)
Resp = TypeVar("Resp", bound=Message)

log = logging.getLogger("reef")

_UNCHANGED: Any = object()


_SETTLED = (
    grpc.ChannelConnectivity.READY,
    grpc.ChannelConnectivity.TRANSIENT_FAILURE,
    grpc.ChannelConnectivity.SHUTDOWN,
)


def _serialize(message: Message) -> bytes:
    return message.SerializeToString()


class _Transport:
    """One asyncio channel, on the loop that made it, shared by a client and its copies."""

    def __init__(self, spec: _channel.Spec, connect_timeout: float) -> None:
        self.spec = spec
        self.connect_timeout = connect_timeout
        self.loop = asyncio.get_running_loop()
        self.channel = spec.open_aio()
        self.repin_lock = asyncio.Lock()
        self.callables: dict[str, Any] = {}
        self.status: GetDaemonStatusResponse | None = None
        self.closed = False

    def check_loop(self) -> None:
        try:
            running = asyncio.get_running_loop()
        except RuntimeError:
            running = None
        if running is not self.loop:
            raise RuntimeError(
                "this AsyncClient belongs to another event loop; connect again on this one"
            )

    async def wait_ready(self) -> None:
        """Waits until the channel is connected; fails as soon as connecting does."""

        async def settled() -> grpc.ChannelConnectivity:
            state = self.channel.get_state(try_to_connect=True)
            while state not in _SETTLED:
                await self.channel.wait_for_state_change(state)
                state = self.channel.get_state(try_to_connect=True)
            return state

        try:
            state: grpc.ChannelConnectivity | None = await asyncio.wait_for(
                settled(), self.connect_timeout
            )
        except TimeoutError:
            state = None
        if state == grpc.ChannelConnectivity.READY:
            return
        await self.channel.close()
        raise _connect.connect_error(self.spec, state is not None, self.connect_timeout)

    def callable(self, rpc: Rpc[Any, Any]) -> Any:
        found = self.callables.get(rpc.path)
        if found is None:
            factory = self.channel.unary_stream if rpc.streaming else self.channel.unary_unary
            found = factory(
                rpc.path,
                request_serializer=_serialize,
                response_deserializer=rpc.response.FromString,
            )
            self.callables[rpc.path] = found
        return found

    async def raw(self, path: str, request: Message, response: type[Message]) -> Any:
        call = self.channel.unary_unary(
            path, request_serializer=_serialize, response_deserializer=response.FromString
        )
        return await call(request, timeout=5.0)

    async def repin(self, seen: _channel.Spec) -> bool:
        """:meth:`reef.client._Transport.repin` for asyncio: one task re-pins at a time."""
        async with self.repin_lock:
            if self.spec is not seen:
                return True
            spec = await asyncio.to_thread(_channel.repinned, seen, self.connect_timeout)
            if spec is None:
                return False
            old = self.channel
            self.spec = spec
            self.channel = spec.open_aio()
            self.callables.clear()
        await old.close()
        log.debug("re-pinned %s after a certificate renewal", spec.label)
        return True

    async def close(self) -> None:
        if self.closed:
            return
        self.closed = True
        await self.channel.close()


class AsyncClient(Methods):
    """A connection to a Reef daemon for asyncio, with one awaitable method per RPC.

    Made by :func:`reef.aio.connect`; belongs to the loop it was made on. An async context
    manager that closes the connection. Streams are async iterators and async context managers;
    page iterators are async generators.
    """

    def __init__(self, transport: _Transport, *, agent: str | None, timeout: float | None) -> None:
        self._transport = transport
        self._agent = agent
        self._timeout = timeout

    @property
    def target(self) -> str:
        """The daemon's socket path, or the remote daemon's origin."""
        return self._transport.spec.label

    @property
    def agent(self) -> str | None:
        """The name sent as ``reef-agent`` on every call, if any."""
        return self._agent

    @property
    def remote(self) -> bool:
        """Whether this is a remote daemon."""
        return self._transport.spec.remote is not None

    def with_options(
        self,
        *,
        timeout: float | None = _UNCHANGED,
        agent: str | None = _UNCHANGED,
    ) -> AsyncClient:
        """A client sharing this one's connection, with another deadline or agent."""
        return AsyncClient(
            self._transport,
            agent=self._agent if agent is _UNCHANGED else _identity.agent_from(agent or ""),
            timeout=self._timeout if timeout is _UNCHANGED else timeout,
        )

    async def close(self) -> None:
        """Closes the connection, for every client sharing it."""
        await self._transport.close()

    async def __aenter__(self) -> AsyncClient:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        await self.close()

    def _metadata(self) -> tuple[tuple[str, str], ...]:
        if self._agent:
            return ((_identity.AGENT_METADATA, self._agent),)
        return ()

    def _check(self, rpc: Rpc[Any, Any], request: Message) -> None:
        self._transport.check_loop()
        size = request.ByteSize()
        if size > _channel.MAX_MESSAGE_BYTES:
            raise InvalidArgumentError(
                f"the request is {size} bytes; the daemon accepts at most "
                f"{_channel.MAX_MESSAGE_BYTES}",
                hint="Edit large files in a workspace, or split the edits across requests.",
                method=rpc.path,
            )

    async def _unary(self, rpc: Rpc[Req, Resp], request: Req) -> Resp:
        self._check(rpc, request)
        retried = False
        while True:
            spec = self._transport.spec
            call = self._transport.callable(rpc)
            try:
                response: Resp = await call(
                    request, timeout=self._timeout, metadata=self._metadata()
                )
            except grpc.RpcError as error:
                if not retried and _connect.needs_repin(spec, error):
                    retried = True
                    # A read is retried once on the new channel; a write may have landed.
                    if await self._transport.repin(spec) and rpc.access == "READ":
                        continue
                raise await self._error(rpc, error) from None
            return response

    def _stream(self, rpc: Rpc[Req, Resp], request: Req) -> AsyncStream[Resp]:
        self._check(rpc, request)
        spec = self._transport.spec
        call = self._transport.callable(rpc)(request, metadata=self._metadata())

        async def error(failure: grpc.RpcError) -> ReefError:
            # On a pinned channel, re-pin first, so the next stream works; then the same
            # mapping (and incompatible-daemon probe) as a unary call's.
            if _connect.needs_repin(spec, failure):
                try:
                    await self._transport.repin(spec)
                except ReefError as refused:
                    return refused
            return await self._error(rpc, failure)

        return AsyncStream(call, error)

    async def _error(self, rpc: Rpc[Any, Any], error: grpc.RpcError) -> ReefError:
        misdirected = _connect.misdirected(self._transport.spec, error)
        if misdirected is not None:
            return misdirected
        if _connect.is_unserved(error):
            return await self._probe(rpc.path, error)
        return from_rpc_error(error, rpc.path)

    async def _probe(self, method: str, error: grpc.RpcError) -> ReefError:
        from reef.api.v1 import GetDaemonStatusRequest, GetDaemonStatusResponse
        from reef.daemon.v0 import daemon_pb2

        try:
            status = await self._transport.raw(
                _connect.DAEMON_STATUS,
                GetDaemonStatusRequest(skip_backends=True),
                GetDaemonStatusResponse,
            )
            return _connect.incompatible(self._transport.spec, method, error, status, None)
        except grpc.RpcError:
            pass
        try:
            v0 = await self._transport.raw(
                _connect.V0_STATUS, daemon_pb2.StatusRequest(), daemon_pb2.StatusResponse
            )
        except grpc.RpcError:
            v0 = None
        return _connect.incompatible(self._transport.spec, method, error, None, v0)

    async def daemon_status(self, *, refresh: bool = False) -> GetDaemonStatusResponse:
        """``GetDaemonStatus`` without the backend probe, cached for the connection's life."""
        if self._transport.status is None or refresh:
            self._transport.status = await self.get_daemon_status(skip_backends=True)
        return self._transport.status

    async def require_revision(self, revision: int) -> None:
        """Checks that the daemon serves API revision ``revision`` or later.

        Raises:
            IncompatibleError: The daemon's revision is lower.
        """
        _connect.require(self._transport.spec, revision, await self.daemon_status())

    def workspace(
        self,
        repo: str,
        *,
        change: str | None = None,
        new_on: str | None = None,
        purpose: str | None = None,
        owner: str | None = None,
        backend: Any = None,
        lease_length: Any = None,
        activity: str | None = None,
        keep: bool = False,
        report_presence: bool = True,
    ) -> AsyncWorkspaceBlock:
        """A new workspace for an ``async with`` block; see :meth:`reef.Client.workspace`."""
        from reef.aio.workspace import AsyncWorkspaceBlock

        return AsyncWorkspaceBlock(
            self,
            {
                "repo": repo,
                "change": change,
                "new_on": new_on,
                "purpose": purpose,
                "owner": owner,
                "backend": backend,
                "lease_length": lease_length,
            },
            activity=activity,
            keep=keep,
            report_presence=report_presence,
            release=True,
        )

    def attach(
        self,
        workspace: str,
        *,
        owner: str | None = None,
        release: bool = False,
        report_presence: bool = True,
        activity: str | None = None,
    ) -> AsyncWorkspaceBlock:
        """An existing workspace for an ``async with`` block; see :meth:`reef.Client.attach`."""
        from reef.aio.workspace import AsyncWorkspaceBlock

        return AsyncWorkspaceBlock(
            self,
            None,
            attach=workspace,
            owner=owner,
            activity=activity,
            keep=not release,
            report_presence=report_presence,
            release=release,
        )


async def connect(
    socket: str | os.PathLike[str] | None = None,
    *,
    remote: str | None = None,
    token: str | None = None,
    fingerprint: str | None = None,
    state_dir: str | os.PathLike[str] | None = None,
    config: str | os.PathLike[str] | None = None,
    agent: str | None = None,
    timeout: float | None = None,
    connect_timeout: float = 5.0,
    root_certificates: bytes | None = None,
) -> AsyncClient:
    """Connects to a Reef daemon on the running loop; the arguments are
    :func:`reef.connect`'s. Never starts a daemon.
    """
    checked_agent = _identity.agent_from(agent)
    spec = await asyncio.to_thread(
        _connect.spec_for,
        socket=socket,
        remote=remote,
        token=token,
        fingerprint=fingerprint,
        state_dir=state_dir,
        config=config,
        connect_timeout=connect_timeout,
        root_certificates=root_certificates,
    )
    transport = _Transport(spec, connect_timeout)
    await transport.wait_ready()
    return AsyncClient(transport, agent=checked_agent, timeout=timeout)
