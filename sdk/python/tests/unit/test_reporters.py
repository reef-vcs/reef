"""The real reporters, sync and asyncio, against an in-process daemon that answers or hangs."""

from __future__ import annotations

import asyncio
import threading
import time
from collections.abc import Callable, Iterator
from concurrent import futures
from pathlib import Path
from typing import Any

import grpc
import pytest

import reef
import reef.aio
import reef.aio.presence
from reef.api.v1 import ClearPresenceResponse, Presence, ReportPresenceResponse
from reef.presence import Reporter, State, Subject

ACTIVITY = 1


class Daemon:
    """Answers ReportPresence and ClearPresence, or hangs on them once ``hang`` is set."""

    def __init__(self) -> None:
        self.reports: list[bytes] = []
        self.clears: list[bytes] = []
        self.hang = threading.Event()
        self.release = threading.Event()

    def handler(self, path: str) -> grpc.RpcMethodHandler | None:
        def report(request: bytes, context: grpc.ServicerContext) -> ReportPresenceResponse:
            self.reports.append(request)
            if self.hang.is_set():
                self.release.wait(10)
            return ReportPresenceResponse(presence=Presence(id="0000000000000001"))

        def clear(request: bytes, context: grpc.ServicerContext) -> ClearPresenceResponse:
            self.clears.append(request)
            if self.hang.is_set():
                self.release.wait(10)
            return ClearPresenceResponse()

        handlers = {
            "/reef.api.v1.PresenceService/ReportPresence": report,
            "/reef.api.v1.PresenceService/ClearPresence": clear,
        }
        if path not in handlers:
            return None
        return grpc.unary_unary_rpc_method_handler(
            handlers[path],
            request_deserializer=lambda b: b,
            response_serializer=lambda m: m.SerializeToString(),
        )


@pytest.fixture
def daemon(short_tmp: Path) -> Iterator[tuple[Daemon, str]]:
    fake = Daemon()

    class Generic(grpc.GenericRpcHandler):
        def service(self, details: grpc.HandlerCallDetails) -> grpc.RpcMethodHandler | None:
            return fake.handler(details.method)

    server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
    server.add_generic_rpc_handlers((Generic(),))
    socket = str(short_tmp / "p.sock")
    server.add_insecure_port(f"unix:{socket}")
    server.start()
    yield fake, socket
    fake.release.set()
    server.stop(None)


@pytest.fixture
def thread_errors() -> Iterator[list[BaseException]]:
    seen: list[BaseException] = []
    previous = threading.excepthook

    def hook(args: Any) -> None:
        seen.append(args.exc_value)

    threading.excepthook = hook
    yield seen
    threading.excepthook = previous


def wait_for(condition: Callable[[], bool], timeout: float = 5) -> None:
    deadline = time.monotonic() + timeout
    while not condition():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.02)


def subject() -> Subject:
    return Subject(agent="bot-1", kind=ACTIVITY, workspace="ws")


def test_sync_reporter_reports_and_clears(daemon: tuple[Daemon, str]) -> None:
    fake, socket = daemon
    with reef.connect(socket) as client:
        reporter = Reporter(client, subject())
        reporter.set(State(activity="a"))
        wait_for(lambda: len(fake.reports) == 1)
        wait_for(reporter.is_live)
        reporter.clear()
    assert len(fake.clears) == 1


def test_sync_clear_is_bounded_and_quiet(
    daemon: tuple[Daemon, str], thread_errors: list[BaseException]
) -> None:
    fake, socket = daemon
    with reef.connect(socket) as client:
        for hang_on in ("report", "clear"):
            fake.reports.clear()
            fake.clears.clear()
            fake.hang.clear()
            if hang_on == "report":
                fake.hang.set()
            reporter = Reporter(client, subject(), clear_deadline=0.5)
            reporter.set(State(activity="a"))
            wait_for(lambda: len(fake.reports) == 1)
            fake.hang.set()
            start = time.monotonic()
            reporter.clear()
            assert time.monotonic() - start < 1.5, hang_on
            time.sleep(0.3)  # the thread finishes after the cancel
            assert thread_errors == [], hang_on


async def test_async_reporter_reports_clears_and_is_bounded(daemon: tuple[Daemon, str]) -> None:
    fake, socket = daemon
    client = await reef.aio.connect(socket)
    async with client:
        reporter = reef.aio.presence.Reporter(client, subject())
        reporter.set(State(activity="a"))
        for _ in range(250):
            if fake.reports:
                break
            await asyncio.sleep(0.02)
        assert fake.reports
        await reporter.clear()
        assert len(fake.clears) == 1

        fake.hang.set()
        reporter = reef.aio.presence.Reporter(client, subject(), clear_deadline=0.5)
        reporter.set(State(activity="b"))
        await asyncio.sleep(0.2)
        start = time.monotonic()
        await reporter.clear()
        assert time.monotonic() - start < 1.5


async def test_async_reporter_survives_a_closed_client(daemon: tuple[Daemon, str]) -> None:
    _, socket = daemon
    client = await reef.aio.connect(socket)
    reporter = reef.aio.presence.Reporter(client, subject())
    await client.close()
    reporter.set(State(activity="after close"))
    await asyncio.sleep(0.2)
    await reporter.clear()  # neither raises nor hangs
