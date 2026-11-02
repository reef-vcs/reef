"""The sync client (R2.06, ADR 0054)."""

from __future__ import annotations

import logging
import os
import threading
from collections.abc import Callable
from types import TracebackType
from typing import TYPE_CHECKING, Any, TypeVar

import grpc
from google.protobuf.message import Message

from reef import _channel, _connect, _identity, _rpcs
from reef._methods import Methods
from reef._rpc import Rpc
from reef._stream import Stream
from reef.errors import (
    InvalidArgumentError,
    ReefError,
    from_rpc_error,
)

if TYPE_CHECKING:
    from reef.api.v1 import GetDaemonStatusResponse
    from reef.workspace import WorkspaceBlock

Req = TypeVar("Req", bound=Message)
Resp = TypeVar("Resp", bound=Message)

log = logging.getLogger("reef")

_UNCHANGED: Any = object()


class _Transport:
    """One channel to one daemon, shared by a client and its :meth:`Client.with_options` copies."""

    def __init__(self, spec: _channel.Spec, connect_timeout: float) -> None:
        self.spec = spec
        self.connect_timeout = connect_timeout
        self.lock = threading.Lock()
        self.repin_lock = threading.Lock()
        self.channel = spec.open()
        self.callables: dict[str, Any] = {}
        self.status: GetDaemonStatusResponse | None = None
        self.closed = False

    def wait_ready(self) -> None:
        """Waits until the channel is connected; fails as soon as connecting does."""
        settled = threading.Event()
        states: list[grpc.ChannelConnectivity] = []

        def watch(state: grpc.ChannelConnectivity) -> None:
            if state in _SETTLED:
                states.append(state)
                settled.set()

        self.channel.subscribe(watch, try_to_connect=True)
        try:
            in_time = settled.wait(self.connect_timeout)
        finally:
            self.channel.unsubscribe(watch)
        if in_time and states[0] == grpc.ChannelConnectivity.READY:
            return
        self.channel.close()
        raise _connect.connect_error(self.spec, in_time, self.connect_timeout)

    def callable(self, rpc: Rpc[Any, Any]) -> Any:
        with self.lock:
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

    def raw(self, path: str, request: Message, response: type[Message], timeout: float) -> Any:
        call = self.channel.unary_unary(
            path, request_serializer=_serialize, response_deserializer=response.FromString
        )
        return call(request, timeout=timeout)

    def repin(self, seen: _channel.Spec) -> bool:
        """Rebuilds the channel if a pinned daemon renewed its certificate; whether the channel
        is now another than the one ``seen`` described. One caller re-pins at a time, and a
        caller that finds the channel already swapped uses the new one.

        Raises:
            RemoteTlsError: The daemon now presents another key.
        """
        with self.repin_lock:
            if self.spec is not seen:
                return True
            spec = _channel.repinned(seen, self.connect_timeout)
            if spec is None:
                return False
            with self.lock:
                old = self.channel
                self.spec = spec
                self.channel = spec.open()
                self.callables.clear()
        old.close()
        log.debug("re-pinned %s after a certificate renewal", spec.label)
        return True

    def close(self) -> None:
        with self.lock:
            if self.closed:
                return
            self.closed = True
        self.channel.close()


_SETTLED = (
    grpc.ChannelConnectivity.READY,
    grpc.ChannelConnectivity.TRANSIENT_FAILURE,
    grpc.ChannelConnectivity.SHUTDOWN,
)


def _serialize(message: Message) -> bytes:
    return message.SerializeToString()


class Client(Methods):
    """A connection to a Reef daemon, with one method per RPC.

    Made by :func:`reef.connect`. Thread-safe; a context manager that closes the connection.
    Every method takes the request's fields as keywords (or a whole request message) and
    returns the response message; the daemon's errors are raised as :class:`reef.ReefError`.
    """

    def __init__(
        self,
        transport: _Transport,
        *,
        agent: str | None,
        timeout: float | None,
    ) -> None:
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
    ) -> Client:
        """A client sharing this one's connection, with another deadline or agent.

        Args:
            timeout: The deadline, in seconds, for each unary call; None for none.
            agent: The agent sent as ``reef-agent``; None or blank for none.
        """
        return Client(
            self._transport,
            agent=self._agent if agent is _UNCHANGED else _identity.agent_from(agent or ""),
            timeout=self._timeout if timeout is _UNCHANGED else timeout,
        )

    def close(self) -> None:
        """Closes the connection, for every client sharing it."""
        self._transport.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()

    def _metadata(self) -> tuple[tuple[str, str], ...]:
        if self._agent:
            return ((_identity.AGENT_METADATA, self._agent),)
        return ()

    def _check_size(self, rpc: Rpc[Any, Any], request: Message) -> None:
        size = request.ByteSize()
        if size > _channel.MAX_MESSAGE_BYTES:
            raise InvalidArgumentError(
                f"the request is {size} bytes; the daemon accepts at most "
                f"{_channel.MAX_MESSAGE_BYTES}",
                hint="Edit large files in a workspace, or split the edits across requests.",
                method=rpc.path,
            )

    def _unary(self, rpc: Rpc[Req, Resp], request: Req) -> Resp:
        self._check_size(rpc, request)
        retried = False
        while True:
            spec = self._transport.spec
            call = self._transport.callable(rpc)
            try:
                response: Resp = call(request, timeout=self._timeout, metadata=self._metadata())
            except grpc.RpcError as error:
                if not retried and _connect.needs_repin(spec, error):
                    retried = True
                    # A read is retried once on the new channel; a write may have landed.
                    if self._transport.repin(spec) and rpc.access == "READ":
                        continue
                raise self._error(rpc, error) from None
            return response

    def _stream(self, rpc: Rpc[Req, Resp], request: Req) -> Stream[Resp]:
        self._check_size(rpc, request)
        spec = self._transport.spec
        call = self._transport.callable(rpc)(request, metadata=self._metadata())
        return Stream(call, lambda error: self._stream_error(rpc, spec, error))

    def _stream_error(
        self, rpc: Rpc[Any, Any], spec: _channel.Spec, error: grpc.RpcError
    ) -> ReefError:
        """A stream's failure; on a pinned channel, re-pins first, so the next stream works."""
        if _connect.needs_repin(spec, error):
            try:
                self._transport.repin(spec)
            except ReefError as refused:
                return refused
        return self._error(rpc, error)

    def _error(self, rpc: Rpc[Any, Any], error: grpc.RpcError) -> ReefError:
        spec = self._transport.spec
        misdirected = _connect.misdirected(spec, error)
        if misdirected is not None:
            return misdirected
        if _connect.is_unserved(error):
            return self._probe(rpc.path, error)
        return from_rpc_error(error, rpc.path)

    def _probe(self, method: str, error: grpc.RpcError) -> ReefError:
        """What a daemon that could not serve ``method`` is: two calls at most."""
        from reef.api.v1 import GetDaemonStatusRequest, GetDaemonStatusResponse
        from reef.daemon.v0 import daemon_pb2

        metadata = self._metadata()
        try:
            status = self._transport.raw(
                _connect.DAEMON_STATUS,
                GetDaemonStatusRequest(skip_backends=True),
                GetDaemonStatusResponse,
                timeout=5.0,
            )
            return _connect.incompatible(self._transport.spec, method, error, status, None)
        except grpc.RpcError:
            pass
        del metadata
        try:
            v0 = self._transport.raw(
                _connect.V0_STATUS, daemon_pb2.StatusRequest(), daemon_pb2.StatusResponse, 5.0
            )
        except grpc.RpcError:
            v0 = None
        return _connect.incompatible(self._transport.spec, method, error, None, v0)

    def daemon_status(self, *, refresh: bool = False) -> GetDaemonStatusResponse:
        """``GetDaemonStatus`` without the backend probe, cached for the connection's life."""
        if self._transport.status is None or refresh:
            self._transport.status = self.get_daemon_status(skip_backends=True)
        return self._transport.status

    def require_revision(self, revision: int) -> None:
        """Checks that the daemon serves API revision ``revision`` or later.

        Call it before sending a field or RPC added after revision 1: a daemon ignores fields it
        does not know.

        Raises:
            IncompatibleError: The daemon's revision is lower.
        """
        _connect.require(self._transport.spec, revision, self.daemon_status())

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
    ) -> WorkspaceBlock:
        """A new workspace for a ``with`` block, as ``reef ws run`` holds one for a command.

        Entering creates the workspace, renews its lease while the block runs and reports one
        presence entry for it; leaving clears the entry and releases the workspace with a final
        snapshot, so every edit is kept, unless ``keep``.

        Args:
            repo: The repository: its id, or a path in it.
            change: Edit this change.
            new_on: A new change on this revision; with neither, the daemon's default start.
            purpose: Free text, shown in ``reef ws list``.
            owner: The owner; ``<user>@<host>:<pid>`` by default, as ``ws run``'s.
            backend: A ``Backend`` value or name.
            lease_length: A ``timedelta``; the daemon's default when None.
            activity: The presence entry's text; the purpose, else the command line.
            keep: Leave the workspace active at the end instead of releasing it.
            report_presence: Report a presence entry for the block.
        """
        from reef.workspace import WorkspaceBlock

        return WorkspaceBlock(
            self,
            dict(
                repo=repo,
                change=change,
                new_on=new_on,
                purpose=purpose,
                owner=owner,
                backend=backend,
                lease_length=lease_length,
            ),
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
    ) -> WorkspaceBlock:
        """An existing workspace for a ``with`` block: its lease renewed and its presence
        reported while the block runs, and released at the end only with ``release``.

        Args:
            workspace: The workspace's id.
            owner: Its holder; without one, it must be held by this user on this machine
                (``<user>@<host>``, with or without a ``:<pid>``), as ``reef ws run`` requires.
            release: Release it, with a final snapshot, when the block ends.
            report_presence: Report a presence entry for the block.
            activity: The presence entry's text; the workspace's purpose by default.
        """
        from reef.workspace import WorkspaceBlock

        return WorkspaceBlock(
            self,
            None,
            attach=workspace,
            owner=owner,
            activity=activity,
            keep=not release,
            report_presence=report_presence,
            release=release,
        )


def connect(
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
) -> Client:
    """Connects to a Reef daemon, as the ``reef`` CLI finds it. Never starts one.

    With no arguments, the daemon is the one ``REEF_REMOTE`` names, else the one on the socket
    the CLI uses (``REEF_SOCKET``, ``REEF_STATE_DIR``, ``REEF_CONFIG``, then the platform's
    default).

    Args:
        socket: The daemon's socket.
        remote: A remote daemon: a profile's name in ``<state>/remotes.toml``, or an
            ``https://`` URL with ``token``.
        token: The remote's token; ``REEF_REMOTE_TOKEN`` by default.
        fingerprint: The remote's pinned key, ``sha256:...``; ``REEF_REMOTE_FINGERPRINT`` by
            default. Without one, the remote's certificate must validate against grpc's roots
            (or ``root_certificates``).
        state_dir: The state directory, for the socket and the remote profiles.
        config: The configuration file, for ``[daemon] socket``.
        agent: The agent name sent as ``reef-agent`` on every call; ``REEF_AGENT`` by default.
        timeout: The default deadline, in seconds, for each unary call; none by default.
        connect_timeout: How long to wait for the connection, in seconds.
        root_certificates: PEM roots for a remote daemon with a certificate from a private CA.

    Raises:
        DaemonNotRunningError: Nothing answered on the socket.
        RemoteConnectError: Nothing answered at the remote.
        RemoteTlsError: The remote's certificate was refused; nothing was sent.
        ConfigError: The socket or the remote could not be settled.
        InvalidArgumentError: The agent name would be refused.
    """
    checked_agent = _identity.agent_from(agent)
    spec = _connect.spec_for(
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
    transport.wait_ready()
    return Client(transport, agent=checked_agent, timeout=timeout)


_ = (_rpcs, Callable)
