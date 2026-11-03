"""The incompatible-daemon probe, against an in-process fake daemon on a scratch socket."""

from __future__ import annotations

from collections.abc import Callable, Iterator
from concurrent import futures
from pathlib import Path
from typing import Any

import grpc
import pytest
from google.protobuf import any_pb2
from google.rpc import status_pb2
from grpc_status import rpc_status

import reef
import reef.aio
from reef._build import CARGO_VERSION
from reef.api.v1 import ErrorDetail, GetDaemonStatusResponse, ListRepositoriesResponse
from reef.daemon.v0 import daemon_pb2


def unimplemented(reason: str | None) -> Callable[[Any, grpc.ServicerContext], Any]:
    def handler(request: Any, context: grpc.ServicerContext) -> Any:
        if reason is None:
            context.abort(grpc.StatusCode.UNIMPLEMENTED, "unknown method")
        detail = any_pb2.Any()
        detail.Pack(ErrorDetail(kind=9, reason=reason))  # type: ignore[arg-type]
        context.abort_with_status(
            rpc_status.to_status(
                status_pb2.Status(code=12, message="not implemented", details=[detail])
            )
        )

    return handler


class Fake:
    """A daemon answering ListRepositories, v1's GetDaemonStatus and v0's Status by script."""

    def __init__(self, method: Any, v1: Any, v0: Any) -> None:
        self.calls: list[str] = []
        self.answers = {
            "/reef.api.v1.RepositoryService/ListRepositories": method,
            "/reef.api.v1.DaemonService/GetDaemonStatus": v1,
            "/reef.daemon.v0.Daemon/Status": v0,
        }

    def handler(self, path: str) -> grpc.RpcMethodHandler:
        answer = self.answers[path]

        def handle(request: Any, context: grpc.ServicerContext) -> Any:
            self.calls.append(path)
            if callable(answer):
                return answer(request, context)
            return answer

        return grpc.unary_unary_rpc_method_handler(
            handle,
            request_deserializer=lambda b: b,
            response_serializer=lambda m: m.SerializeToString(),
        )


@pytest.fixture
def serve(short_tmp: Path) -> Iterator[Callable[[Fake], str]]:
    servers: list[grpc.Server] = []

    def start(fake: Fake) -> str:
        socket = str(short_tmp / f"d{len(servers)}.sock")

        class Generic(grpc.GenericRpcHandler):
            def service(self, details: grpc.HandlerCallDetails) -> grpc.RpcMethodHandler | None:
                if details.method in fake.answers:
                    return fake.handler(details.method)
                return None

        server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
        server.add_generic_rpc_handlers((Generic(),))
        server.add_insecure_port(f"unix:{socket}")
        server.start()
        servers.append(server)
        return socket

    yield start
    for server in servers:
        server.stop(None)


def other_v1() -> GetDaemonStatusResponse:
    return GetDaemonStatusResponse(version="9.9.9", api_revision=99, active_workspace_count=2)


CASES = {
    "a v1 daemon of another version": (
        Fake(unimplemented(None), other_v1(), unimplemented(None)),
        ("v1", "9.9.9", 2),
        2,
    ),
    "a v0 daemon": (
        Fake(
            unimplemented("UNKNOWN_METHOD"),
            unimplemented(None),
            daemon_pb2.StatusResponse(version="0.1.0", active_workspaces=1),
        ),
        ("v0", "0.1.0", 1),
        3,
    ),
    "the same version, not implemented": (
        Fake(
            unimplemented("NOT_IMPLEMENTED"),
            GetDaemonStatusResponse(version=CARGO_VERSION),
            unimplemented(None),
        ),
        None,
        2,
    ),
    "a daemon answering neither": (
        Fake(unimplemented(None), unimplemented(None), unimplemented(None)),
        None,
        3,
    ),
}


def check(error: reef.ReefError, expected: tuple[str, str, int] | None) -> None:
    if expected is None:
        assert type(error) is reef.UnsupportedError
        return
    assert isinstance(error, reef.IncompatibleError)
    assert (error.daemon_api, error.daemon_version, error.active_workspaces) == expected
    assert error.reason == "API_VERSION_MISMATCH"


@pytest.mark.parametrize("name", list(CASES))
def test_incompatible_probe(name: str, serve: Callable[[Fake], str]) -> None:
    fake, expected, calls = CASES[name]
    fake.calls.clear()
    socket = serve(fake)
    with reef.connect(socket) as client, pytest.raises(reef.ReefError) as raised:
        client.list_repositories()
    check(raised.value, expected)
    assert len(fake.calls) == calls, "the call, then at most two probe calls"


@pytest.mark.parametrize("name", list(CASES))
async def test_incompatible_probe_async(name: str, serve: Callable[[Fake], str]) -> None:
    fake, expected, calls = CASES[name]
    fake.calls.clear()
    socket = serve(fake)
    client = await reef.aio.connect(socket)
    async with client:
        with pytest.raises(reef.ReefError) as raised:
            await client.list_repositories()
    check(raised.value, expected)
    assert len(fake.calls) == calls


def test_answers_pass_through(serve: Callable[[Fake], str]) -> None:
    fake = Fake(ListRepositoriesResponse(next_page_token=""), other_v1(), unimplemented(None))
    socket = serve(fake)
    with reef.connect(socket) as client:
        assert client.list_repositories() == ListRepositoriesResponse()
        assert client.daemon_status().version == "9.9.9"
        client.require_revision(14)
        with pytest.raises(reef.IncompatibleError):
            client.require_revision(100)
