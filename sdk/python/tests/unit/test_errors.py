"""The daemon's error statuses as exceptions (ADR 0029's error model)."""

from __future__ import annotations

import grpc
import pytest
from google.protobuf import any_pb2
from google.rpc import status_pb2

import reef
from reef import errors
from reef.api.v1 import ErrorDetail
from reef.errors import ErrorKind

KINDS = [
    (1, ErrorKind.NOT_FOUND, reef.NotFoundError, grpc.StatusCode.NOT_FOUND),
    (2, ErrorKind.ALREADY_EXISTS, reef.AlreadyExistsError, grpc.StatusCode.ALREADY_EXISTS),
    (3, ErrorKind.INVALID_ARGUMENT, reef.InvalidArgumentError, grpc.StatusCode.INVALID_ARGUMENT),
    (4, ErrorKind.CONFLICT, reef.ConflictError, grpc.StatusCode.FAILED_PRECONDITION),
    (5, ErrorKind.UNAVAILABLE, reef.UnavailableError, grpc.StatusCode.UNAVAILABLE),
    (6, ErrorKind.PERMISSION_DENIED, reef.PermissionDeniedError, grpc.StatusCode.PERMISSION_DENIED),
    (7, ErrorKind.IO, reef.IoError, grpc.StatusCode.INTERNAL),
    (8, ErrorKind.INTERNAL, reef.InternalError, grpc.StatusCode.INTERNAL),
    (9, ErrorKind.UNSUPPORTED, reef.UnsupportedError, grpc.StatusCode.UNIMPLEMENTED),
]


def trailing(detail: ErrorDetail, code: grpc.StatusCode) -> list[tuple[str, bytes]]:
    packed = any_pb2.Any()
    packed.Pack(detail)
    status = status_pb2.Status(code=code.value[0], message="m", details=[packed])
    return [("grpc-status-details-bin", status.SerializeToString())]


@pytest.mark.parametrize(("number", "kind", "cls", "code"), KINDS)
def test_error_mapping(
    number: int, kind: ErrorKind, cls: type[reef.ReefError], code: grpc.StatusCode
) -> None:
    detail = ErrorDetail(
        kind=number,  # type: ignore[arg-type]
        reason="SOME_REASON",
        chain=["outer", "inner"],
        hint="Do this.",
        next=["reef", "ws", "list"],
        metadata={"workspace": "abcd"},
    )
    error = errors.from_status(code, "outer: inner", trailing(detail, code), "/x/Y")
    assert type(error) is cls
    assert error.kind == kind
    assert (error.reason, error.chain, error.hint, error.next) == (
        "SOME_REASON",
        ["outer", "inner"],
        "Do this.",
        ["reef", "ws", "list"],
    )
    assert error.metadata == {"workspace": "abcd"}
    assert (error.code, error.method) == (code, "/x/Y")
    assert str(error) == "outer: inner\nDo this."

    # Without a detail, the code decides; IO has no code of its own.
    bare = errors.from_status(code, "bare", None)
    expected = ErrorKind.INTERNAL if kind == ErrorKind.IO else kind
    assert bare.kind == expected
    assert (bare.reason, bare.hint, bare.next, str(bare)) == ("", "", [], "bare")


def test_unspecified_and_unknown_kinds_are_internal() -> None:
    for number in (0, 42):
        detail = ErrorDetail(kind=number)  # type: ignore[arg-type]
        code = grpc.StatusCode.INTERNAL
        error = errors.from_status(code, "x", trailing(detail, code))
        assert type(error) is reef.InternalError
    error = errors.from_status(grpc.StatusCode.UNKNOWN, "x", None)
    assert error.kind == ErrorKind.INTERNAL


def test_local_size_limit_is_invalid_argument() -> None:
    error = errors.from_status(
        grpc.StatusCode.RESOURCE_EXHAUSTED, "Sent message larger than max (5 vs. 4)", None
    )
    assert type(error) is reef.InvalidArgumentError
    assert error.hint == errors.TOO_LARGE_HINT


def test_client_side_errors() -> None:
    error = reef.DaemonNotRunningError("/s/daemon.sock", "connection refused")
    assert isinstance(error, reef.UnavailableError)
    assert error.next == ["reef", "daemon", "start"]
    assert error.chain == ["connect to the daemon at /s/daemon.sock", "connection refused"]

    pin = "sha256:" + "ab" * 32
    seen = "sha256:" + "cd" * 32
    tls = reef.RemoteTlsError("https://vm:1", "pin_mismatch", presented=seen, expected=pin)
    assert isinstance(tls, reef.PermissionDeniedError)
    assert seen in tls.hint and tls.failure == "pin_mismatch"

    misdirected = reef.RemoteMisdirectedError("https://vm:1", "vm")
    assert misdirected.kind == ErrorKind.NOT_FOUND and "`vm`" in misdirected.hint

    incompatible = reef.IncompatibleError(
        "/s",
        "/reef.api.v1.X/Y",
        daemon_version="0.9.0",
        daemon_api="v1",
        active_workspaces=2,
        sdk_version="0.3.0",
    )
    assert incompatible.reason == "API_VERSION_MISMATCH"
    assert incompatible.next == ["reef", "daemon", "stop"]
    assert "reef-vcs==0.9.0" in incompatible.hint
    assert isinstance(incompatible, reef.UnsupportedError)


def test_errors_pickle() -> None:
    import pickle

    tls = reef.RemoteTlsError("https://vm:1", "pin_mismatch", presented="p", expected="e")
    for error in (
        tls,
        reef.IncompatibleError(
            "/s", "/m", daemon_version="9", daemon_api="v1", active_workspaces=1, sdk_version="1"
        ),
        reef.DaemonNotRunningError("/s"),
        errors.from_status(grpc.StatusCode.NOT_FOUND, "gone", None),
    ):
        again = pickle.loads(pickle.dumps(error))
        assert type(again) is type(error)
        assert again.__dict__ == error.__dict__
        assert str(again) == str(error)


def test_received_size_limit_keeps_the_kind() -> None:
    error = errors.from_status(
        grpc.StatusCode.RESOURCE_EXHAUSTED, "Received message larger than max (5 vs. 4)", None
    )
    assert error.hint != errors.TOO_LARGE_HINT
