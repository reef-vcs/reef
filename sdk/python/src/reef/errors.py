"""The daemon's errors as exceptions (ADR 0029, ADR 0054).

Every failure is a :class:`ReefError` of one class per kind. An error the daemon sent carries its
``ErrorDetail``: ``reason`` (such as ``WORKSPACE_NOT_FOUND``), ``hint`` (one sentence on how to fix
it), ``next`` (a ``reef`` command line that does, as an argv list) and ``metadata``. Failures found
by the SDK itself (no daemon on the socket, a refused certificate, a mismatched daemon) are
subclasses with their own advice.
"""

from __future__ import annotations

import enum
from collections.abc import Mapping, Sequence
from typing import Any, ClassVar

import grpc


class ErrorKind(enum.StrEnum):
    """What kind of failure an error is, in the CLI's words."""

    NOT_FOUND = "not_found"
    ALREADY_EXISTS = "already_exists"
    INVALID_ARGUMENT = "invalid_argument"
    CONFLICT = "conflict"
    UNAVAILABLE = "unavailable"
    PERMISSION_DENIED = "permission_denied"
    IO = "io"
    INTERNAL = "internal"
    UNSUPPORTED = "unsupported"


class ReefError(Exception):
    """A failure of a call to the daemon, or to reach it.

    Attributes:
        kind: The error's kind.
        reason: The specific condition, such as ``WORKSPACE_NOT_FOUND``; empty when none.
        message: The error's message: the daemon's chain joined with ``": "``.
        chain: The error's message and each cause's, outermost first.
        hint: How to fix it, in a sentence; empty when none.
        next: A ``reef`` command line that fixes it, as argv; empty when none.
        metadata: Values that go with ``reason``, such as ``{"workspace": "..."}``.
        code: The gRPC status code, or None for a failure found before a call.
        method: The RPC's full path, or None.
    """

    kind: ErrorKind = ErrorKind.INTERNAL
    default_kind: ClassVar[ErrorKind] = ErrorKind.INTERNAL

    def __init__(
        self,
        message: str,
        *,
        kind: ErrorKind | None = None,
        reason: str = "",
        chain: Sequence[str] = (),
        hint: str = "",
        next: Sequence[str] = (),
        metadata: Mapping[str, str] | None = None,
        code: grpc.StatusCode | None = None,
        method: str | None = None,
    ) -> None:
        super().__init__(message)
        self.kind = kind or self.default_kind
        self.message = message
        self.reason = reason
        self.chain = list(chain) or ([message] if message else [])
        self.hint = hint
        self.next = list(next)
        self.metadata = dict(metadata or {})
        self.code = code
        self.method = method

    def __str__(self) -> str:
        if self.hint:
            return f"{self.message}\n{self.hint}"
        return self.message

    def __reduce__(self) -> tuple[Any, ...]:
        # Every field is in __dict__, so subclasses with other constructors pickle too.
        return (_rebuild, (type(self), self.message), self.__dict__.copy())

    def __repr__(self) -> str:
        fields = [f"{self.message!r}", f"kind={self.kind.value!r}"]
        if self.reason:
            fields.append(f"reason={self.reason!r}")
        return f"{type(self).__name__}({', '.join(fields)})"


def _rebuild(cls: type[ReefError], message: str) -> ReefError:
    error = cls.__new__(cls)
    Exception.__init__(error, message)
    return error


class NotFoundError(ReefError):
    """The thing named does not exist."""

    default_kind = ErrorKind.NOT_FOUND


class AlreadyExistsError(ReefError):
    """The thing to create exists already."""

    default_kind = ErrorKind.ALREADY_EXISTS


class InvalidArgumentError(ReefError):
    """The request is wrong as sent."""

    default_kind = ErrorKind.INVALID_ARGUMENT


class ConflictError(ReefError):
    """The request conflicts with the current state (gRPC ``FAILED_PRECONDITION``)."""

    default_kind = ErrorKind.CONFLICT


class UnavailableError(ReefError):
    """The daemon, or something it needs, is not available now; retrying may succeed."""

    default_kind = ErrorKind.UNAVAILABLE


class PermissionDeniedError(ReefError):
    """The caller may not do this."""

    default_kind = ErrorKind.PERMISSION_DENIED


class IoError(ReefError):
    """A filesystem failure on the daemon's machine."""

    default_kind = ErrorKind.IO


class InternalError(ReefError):
    """A bug, or a failure the daemon did not classify."""

    default_kind = ErrorKind.INTERNAL


class UnsupportedError(ReefError):
    """The daemon cannot do this here (gRPC ``UNIMPLEMENTED``)."""

    default_kind = ErrorKind.UNSUPPORTED


class DaemonNotRunningError(UnavailableError):
    """Nothing answered on the daemon's socket. The SDK never starts a daemon."""

    def __init__(self, socket: str, cause: str = "") -> None:
        message = f"connect to the daemon at {socket}"
        super().__init__(
            message,
            chain=[message, cause] if cause else [message],
            hint=(
                "Start the daemon: `reef daemon start`, or run any `reef` command, which starts it."
            ),
            next=["reef", "daemon", "start"],
        )
        self.socket = socket


class RemoteConnectError(UnavailableError):
    """Nothing answered at a remote daemon's URL."""

    def __init__(self, url: str, cause: str = "") -> None:
        message = f"connect to the daemon at {url}"
        super().__init__(
            message,
            chain=[message, cause] if cause else [message],
            hint=(
                "Nothing answered there: is the daemon running with its remote listener "
                "(`reef daemon status` on that machine says `remote: listening`), and is the port "
                "open to this machine?"
            ),
        )
        self.url = url


class RemoteTlsError(PermissionDeniedError):
    """A remote daemon's certificate was refused, before anything was sent.

    Attributes:
        failure: ``pin_mismatch``, ``untrusted`` or ``handshake``.
        presented: The fingerprint the daemon presented, when it was read.
        expected: The pinned fingerprint, for ``pin_mismatch``.
    """

    def __init__(
        self,
        url: str,
        failure: str,
        *,
        presented: str = "",
        expected: str = "",
        reason: str = "",
        hint: str = "",
    ) -> None:
        given_hint = hint
        if failure == "pin_mismatch":
            cause = f"the daemon presented {presented}, not the pinned {expected}"
            hint = (
                f"The daemon presented {presented}, not the pinned key. If `reef remote show` on "
                f"the daemon's machine prints {presented} (its key changes after a restore), add "
                f"the remote again with --fingerprint {presented}; otherwise something else "
                "answered at that address."
            )
        elif failure == "untrusted":
            shown = presented or "an unread certificate"
            cause = f"the daemon's certificate ({shown}) is not trusted: {reason}"
            hint = (
                "The daemon's certificate is not one this machine trusts. If `reef remote show` "
                f"on the daemon's machine prints {shown}, pin it: pass fingerprint={shown!r}, set "
                "REEF_REMOTE_FINGERPRINT, or add the remote with --fingerprint."
            )
        else:
            cause = f"the TLS handshake failed: {reason}"
            hint = (
                "The TLS handshake failed: check that the URL names the daemon's remote listener."
            )
        message = f"refused the daemon at {url}"
        super().__init__(message, chain=[message, cause], hint=given_hint or hint)
        self.url = url
        self.failure = failure
        self.presented = presented
        self.expected = expected


class RemoteMisdirectedError(NotFoundError):
    """A remote daemon answered that it does not serve the URL's host name (HTTP 421)."""

    def __init__(self, url: str, host: str) -> None:
        super().__init__(
            f"the daemon at {url} does not serve the name {host}",
            hint=(
                f"Add `{host}` to `[remote] names` in the daemon's config.toml and restart it, or "
                "connect by a name or IP address it serves (`reef remote show` on that machine "
                "lists them)."
            ),
        )
        self.url = url
        self.host = host


class ConfigError(InvalidArgumentError):
    """The socket, the configuration file or a remote profile could not be settled."""


class IncompatibleError(UnsupportedError):
    """The daemon is another version and cannot serve the call.

    Attributes:
        daemon_version: The daemon's version; ``unknown`` when it would not say.
        daemon_api: ``v0`` (reef 0.1) or ``v1``.
        daemon_revision: The daemon's API revision, when known.
        active_workspaces: Its active workspaces.
    """

    def __init__(
        self,
        target: str,
        method: str,
        *,
        daemon_version: str,
        daemon_api: str,
        active_workspaces: int,
        daemon_revision: int | None = None,
        sdk_version: str,
        detail: str = "",
    ) -> None:
        message = detail or (
            f"the daemon at {target} is reef {daemon_version} (API {daemon_api}) and cannot "
            f"serve {method} for reef-vcs {sdk_version} (API v1)"
        )
        super().__init__(
            message,
            reason="API_VERSION_MISMATCH",
            hint=(
                f"The daemon is reef {daemon_version} (API {daemon_api}); this SDK is reef-vcs "
                f"{sdk_version}. Restart the daemon with the matching reef (`reef daemon stop`, "
                f"then any reef command), or install reef-vcs=={daemon_version}."
            ),
            next=["reef", "daemon", "stop"],
            metadata={"daemon_version": daemon_version, "daemon_api": daemon_api},
            method=method,
        )
        self.daemon_version = daemon_version
        self.daemon_api = daemon_api
        self.daemon_revision = daemon_revision
        self.active_workspaces = active_workspaces


CLASSES: dict[ErrorKind, type[ReefError]] = {
    ErrorKind.NOT_FOUND: NotFoundError,
    ErrorKind.ALREADY_EXISTS: AlreadyExistsError,
    ErrorKind.INVALID_ARGUMENT: InvalidArgumentError,
    ErrorKind.CONFLICT: ConflictError,
    ErrorKind.UNAVAILABLE: UnavailableError,
    ErrorKind.PERMISSION_DENIED: PermissionDeniedError,
    ErrorKind.IO: IoError,
    ErrorKind.INTERNAL: InternalError,
    ErrorKind.UNSUPPORTED: UnsupportedError,
}

# `ErrorDetail.kind`'s numbers (error.proto); 0 and anything unknown read as internal.
_PROTO_KINDS = {
    1: ErrorKind.NOT_FOUND,
    2: ErrorKind.ALREADY_EXISTS,
    3: ErrorKind.INVALID_ARGUMENT,
    4: ErrorKind.CONFLICT,
    5: ErrorKind.UNAVAILABLE,
    6: ErrorKind.PERMISSION_DENIED,
    7: ErrorKind.IO,
    8: ErrorKind.INTERNAL,
    9: ErrorKind.UNSUPPORTED,
}

_CODE_KINDS = {
    grpc.StatusCode.NOT_FOUND: ErrorKind.NOT_FOUND,
    grpc.StatusCode.ALREADY_EXISTS: ErrorKind.ALREADY_EXISTS,
    grpc.StatusCode.INVALID_ARGUMENT: ErrorKind.INVALID_ARGUMENT,
    grpc.StatusCode.FAILED_PRECONDITION: ErrorKind.CONFLICT,
    grpc.StatusCode.UNAVAILABLE: ErrorKind.UNAVAILABLE,
    grpc.StatusCode.PERMISSION_DENIED: ErrorKind.PERMISSION_DENIED,
    grpc.StatusCode.UNIMPLEMENTED: ErrorKind.UNSUPPORTED,
}

DETAIL_TYPE_URL = "type.googleapis.com/reef.api.v1.ErrorDetail"
DETAILS_KEY = "grpc-status-details-bin"
TOO_LARGE_HINT = "Edit large files in a workspace, or split the edits across requests."


def kind_of_code(code: grpc.StatusCode | None) -> ErrorKind:
    """The kind for a status without a Reef detail: the inverse of the daemon's code table."""
    if code is None:
        return ErrorKind.INTERNAL
    return _CODE_KINDS.get(code, ErrorKind.INTERNAL)


def kind_of_proto(kind: int) -> ErrorKind:
    """The kind an ``ErrorDetail.kind`` names; unspecified and unknown values are internal."""
    return _PROTO_KINDS.get(kind, ErrorKind.INTERNAL)


def detail_of(trailing: Any) -> Any | None:
    """The ``reef.api.v1.ErrorDetail`` in a call's trailing metadata, if it carries one."""
    if not trailing:
        return None
    blob: bytes | None = None
    for item in trailing:
        key, value = (item.key, item.value) if hasattr(item, "key") else item
        if key == DETAILS_KEY:
            blob = value if isinstance(value, bytes) else str(value).encode()
            break
    if blob is None:
        return None
    from google.rpc import status_pb2

    from reef.api.v1 import ErrorDetail

    try:
        status = status_pb2.Status.FromString(blob)
    except Exception:
        return None
    for any_detail in status.details:
        if any_detail.type_url == DETAIL_TYPE_URL:
            detail = ErrorDetail()
            if any_detail.Unpack(detail):
                return detail
    return None


def from_status(
    code: grpc.StatusCode | None,
    message: str,
    trailing: Any,
    method: str | None = None,
) -> ReefError:
    """An error status as the exception of its kind, with the daemon's detail when it has one."""
    detail = detail_of(trailing)
    if detail is None:
        kind = kind_of_code(code)
        if code == grpc.StatusCode.RESOURCE_EXHAUSTED and "Sent message larger than max" in message:
            return InvalidArgumentError(message, hint=TOO_LARGE_HINT, code=code, method=method)
        return CLASSES[kind](message, kind=kind, code=code, method=method)
    kind = kind_of_proto(detail.kind)
    return CLASSES[kind](
        message,
        kind=kind,
        reason=detail.reason,
        chain=list(detail.chain),
        hint=detail.hint,
        next=list(detail.next),
        metadata=dict(detail.metadata),
        code=code,
        method=method,
    )


def from_rpc_error(error: grpc.RpcError, method: str | None = None) -> ReefError:
    """A failed call (sync or asyncio) as a :class:`ReefError`."""
    code = error.code() if hasattr(error, "code") else None
    details = error.details() if hasattr(error, "details") else None
    trailing = error.trailing_metadata() if hasattr(error, "trailing_metadata") else None
    return from_status(code, details or str(code), trailing, method)
