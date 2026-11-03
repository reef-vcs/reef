"""Settling what ``connect()`` reaches, shared by the sync and asyncio clients."""

from __future__ import annotations

import os
import re
from typing import Any

import grpc

from reef import _channel, _paths, _remote
from reef._build import CARGO_VERSION
from reef.errors import (
    DETAIL_TYPE_URL,
    DaemonNotRunningError,
    IncompatibleError,
    ReefError,
    RemoteConnectError,
    RemoteMisdirectedError,
    detail_of,
    from_status,
)

V0_STATUS = "/reef.daemon.v0.Daemon/Status"
DAEMON_STATUS = "/reef.api.v1.DaemonService/GetDaemonStatus"
UNSERVED_REASONS = ("UNKNOWN_METHOD", "NOT_IMPLEMENTED")
# The listener's HTTP 421, as grpcio reports it ("Received http2 header with status: 421"), or as
# tonic does ("... HTTP status code 421").
MISDIRECTED = re.compile(r"(header with status: |HTTP status code )421$")

__all__ = ["DETAIL_TYPE_URL", "spec_for"]


def spec_for(
    *,
    socket: str | os.PathLike[str] | None,
    remote: str | None,
    token: str | None,
    fingerprint: str | None,
    state_dir: str | os.PathLike[str] | None,
    config: str | os.PathLike[str] | None,
    connect_timeout: float,
    root_certificates: bytes | None,
) -> _channel.Spec:
    """The socket, or the remote, that ``connect()``'s arguments and the environment name.

    Raises:
        TypeError: Both ``socket`` and ``remote``.
        ConfigError: The paths, or the remote's profile, cannot be settled.
        RemoteConnectError: Nothing answered at the remote.
        RemoteTlsError: The remote's certificate was refused.
    """
    if socket is not None and remote is not None:
        raise TypeError("connect() takes a socket or a remote, not both")
    if socket is None and remote is None:
        from_env = os.environ.get(_remote.REMOTE_ENV, "")
        remote = from_env.strip() or None
    if remote is not None:
        state = _paths.settle_state_dir(state_dir=state_dir)
        target = _remote.choose(remote, state_dir=state, token=token, fingerprint=fingerprint)
        return _channel.remote(
            target, connect_timeout=connect_timeout, root_certificates=root_certificates
        )
    settled = _paths.settle(socket=socket, state_dir=state_dir, config=config)
    return _channel.local(settled.socket)


def connect_error(spec: _channel.Spec, failed: bool, timeout: float) -> ReefError:
    """Why ``connect()`` could not reach the daemon: the connection failed (``failed``), or
    nothing settled within ``timeout``.
    """
    cause = "the connection failed" if failed else f"no connection within {timeout:g} s"
    if spec.remote is not None:
        return RemoteConnectError(spec.label, cause)
    return DaemonNotRunningError(spec.label, cause)


def is_unserved(error: grpc.RpcError) -> bool:
    """Whether an error may say the daemon cannot serve the method: a bare ``UNIMPLEMENTED``,
    or one whose reason is ``UNKNOWN_METHOD`` or ``NOT_IMPLEMENTED``.
    """
    if error.code() != grpc.StatusCode.UNIMPLEMENTED:
        return False
    detail = detail_of(error.trailing_metadata())
    return detail is None or detail.reason in UNSERVED_REASONS


def misdirected(spec: _channel.Spec, error: grpc.RpcError) -> ReefError | None:
    """A remote daemon's HTTP 421 (a host name it does not serve), as its error."""
    if (
        spec.remote is not None
        and error.code() == grpc.StatusCode.UNKNOWN
        and MISDIRECTED.search(error.details() or "")
        and detail_of(error.trailing_metadata()) is None
    ):
        return RemoteMisdirectedError(spec.remote.url.origin, spec.remote.url.host)
    return None


def needs_repin(spec: _channel.Spec, error: grpc.RpcError) -> bool:
    """Whether a failure on a pinned channel may be a renewed certificate."""
    return (
        spec.leaf is not None
        and error.code() == grpc.StatusCode.UNAVAILABLE
        and detail_of(error.trailing_metadata()) is None
    )


def incompatible(
    spec: _channel.Spec,
    method: str,
    error: grpc.RpcError,
    v1: Any | None,
    v0: Any | None,
) -> ReefError:
    """What a daemon that could not serve ``method`` is, from its answers to the two status
    calls (``v1``: ``GetDaemonStatusResponse`` or None; ``v0``: 0.1's ``StatusResponse``).
    """
    original = from_status(error.code(), error.details() or "", error.trailing_metadata(), method)
    if v1 is not None:
        detail = detail_of(error.trailing_metadata())
        same_version = v1.version == CARGO_VERSION
        if same_version and detail is not None and detail.reason == "NOT_IMPLEMENTED":
            return original
        return IncompatibleError(
            spec.label,
            method,
            daemon_version=v1.version or "unknown",
            daemon_api="v1",
            daemon_revision=v1.api_revision,
            active_workspaces=v1.active_workspace_count,
            sdk_version=CARGO_VERSION,
        )
    if v0 is not None:
        return IncompatibleError(
            spec.label,
            method,
            daemon_version=v0.version or "unknown",
            daemon_api="v0",
            active_workspaces=v0.active_workspaces,
            sdk_version=CARGO_VERSION,
        )
    return original


def require(spec: _channel.Spec, revision: int, status: Any) -> None:
    """Raises :class:`IncompatibleError` when the daemon's API revision is below ``revision``."""
    if status.api_revision >= revision:
        return
    raise IncompatibleError(
        spec.label,
        f"API revision {revision}",
        daemon_version=status.version or "unknown",
        daemon_api="v1",
        daemon_revision=status.api_revision,
        active_workspaces=status.active_workspace_count,
        sdk_version=CARGO_VERSION,
        detail=(
            f"the daemon at {spec.label} is reef {status.version} at API revision "
            f"{status.api_revision}; this needs revision {revision}"
        ),
    )
