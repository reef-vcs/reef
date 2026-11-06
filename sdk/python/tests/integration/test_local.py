"""The SDK against a real daemon on its socket."""

from __future__ import annotations

import asyncio
import os
import shutil
import tempfile
import threading
import time
from pathlib import Path

import pytest
from harness import Daemon

import reef
import reef._stream
import reef.aio
from reef._build import CARGO_VERSION
from reef.api.v1 import FileEdit, PresenceKind, WorkspaceState
from reef.presence import Reporter, State, Subject


def wait_for(condition, timeout: float = 5.0, message: str = "condition") -> None:  # type: ignore[no-untyped-def]
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {message}")
        time.sleep(0.05)


def test_connects_through_discovery(daemon: Daemon, monkeypatch: pytest.MonkeyPatch) -> None:
    for name in ("REEF_SOCKET", "REEF_CONFIG", "REEF_REMOTE"):
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("REEF_STATE_DIR", str(daemon.state))
    start = time.perf_counter()
    with reef.connect() as client:
        elapsed = time.perf_counter() - start
        assert client.target == daemon.socket
        assert " " in client.target, "the :authority fix, with a space in the path"
        client.get_daemon_status(skip_backends=True)
    assert elapsed < 1.0


def test_api_revision_matches_the_daemon(client: reef.Client) -> None:
    status = client.daemon_status()
    assert status.version == CARGO_VERSION
    assert status.api_revision == reef.API_REVISION
    client.require_revision(reef.API_REVISION)


def test_daemon_not_running() -> None:
    root = Path(tempfile.mkdtemp(prefix="reef py "))
    try:
        start = time.monotonic()
        with pytest.raises(reef.DaemonNotRunningError) as raised:
            reef.connect(state_dir=root, connect_timeout=1.0)
        assert time.monotonic() - start < 3
        assert raised.value.next == ["reef", "daemon", "start"]
        assert not (root / "daemon.sock").exists(), "no autostart"
    finally:
        shutil.rmtree(root)


def test_errors_carry_the_detail(client: reef.Client, repo: str) -> None:
    with pytest.raises(reef.NotFoundError) as raised:
        client.get_workspace(workspace="abcd0123")
    error = raised.value
    assert error.reason == "WORKSPACE_NOT_FOUND"
    assert error.next == ["reef", "ws", "list", "--all"]
    assert error.hint and error.metadata
    with pytest.raises(reef.InvalidArgumentError) as raised:
        client.list_changes(repo=repo, revset="(((")
    assert raised.value.reason == "INVALID_REVSET"


def test_pages_follow_to_the_end(client: reef.Client, repo: str) -> None:
    with client.workspace(repo, purpose="pages", report_presence=False) as ws:
        for n in range(8):
            (ws.path / f"f{n}.txt").write_text(str(n))
            ws.snapshot()
    paged = [op.id for op in client.list_operations_all(repo=repo, page_size=2)]
    whole = [op.id for op in client.list_operations_all(repo=repo, page_size=1000)]
    assert paged == whole
    assert len(set(paged)) == len(paged) >= 9
    pages = list(
        client.list_workspaces_pages(repo=repo, page_size=1, states=["WORKSPACE_STATE_RELEASED"])
    )
    assert len(pages) >= 1 and pages[-1].next_page_token == ""


def presence_events(stream: reef._stream.Stream, seen: list) -> None:
    for frame in stream:
        seen.extend(frame.events)


def test_workspace_block(client: reef.Client, repo: str) -> None:
    with client.workspace(repo, purpose="the block", new_on="main") as ws:
        assert ws.workspace.owner.endswith(f":{os.getpid()}")
        first = client.get_workspace(workspace=ws.id).workspace.lease.last_heartbeat_at.ToDatetime()
        wait_for(
            lambda: (
                client.get_workspace(workspace=ws.id).workspace.lease.last_heartbeat_at.ToDatetime()
                > first
            ),
            timeout=4,
            message="a heartbeat",
        )
        seen: list = []
        stream = client.subscribe_presence(workspaces=[ws.id])
        watcher = threading.Thread(target=presence_events, args=(stream, seen), daemon=True)
        watcher.start()

        def activity(text: str) -> bool:
            return any(
                event.presence.kind == PresenceKind.PRESENCE_KIND_ACTIVITY
                and event.presence.activity == text
                and not event.ended
                for event in seen
            )

        wait_for(lambda: activity("the block"), message="the presence entry")
        entry = next(e.presence for e in seen if e.presence.activity == "the block")
        assert entry.agent == "sdk-test"
        assert entry.workspace_id == ws.id
        ws.activity("running the tests")
        wait_for(lambda: activity("running the tests"), message="the replaced entry")
        (ws.path / "block.txt").write_text("written in the block\n")
        workspace_id, change = ws.id, ws.change_id
        moved = ws.move(new_on=change)
        assert ws.change_id == moved.workspace.change_id != change, "the handle follows a move"
        (ws.path / "after-move.txt").write_text("on the new change\n")
    released = client.get_workspace(workspace=workspace_id).workspace
    assert released.state == WorkspaceState.WORKSPACE_STATE_RELEASED
    read = client.read_file(repo=repo, revision=change, path="block.txt")
    assert read.content == b"written in the block\n"
    wait_for(
        lambda: any(
            e.ended and e.presence.kind == PresenceKind.PRESENCE_KIND_ACTIVITY for e in seen
        ),
        message="the entry's end",
    )
    stream.cancel()
    watcher.join(5)
    assert not watcher.is_alive()


def test_workspace_block_keep_and_raise(client: reef.Client, repo: str) -> None:
    with client.workspace(repo, purpose="kept", keep=True, report_presence=False) as ws:
        kept = ws.id
    assert (
        client.get_workspace(workspace=kept).workspace.state
        == WorkspaceState.WORKSPACE_STATE_ACTIVE
    )
    client.release_workspace(workspace=kept)

    block = client.workspace(repo, purpose="raises", report_presence=False)
    with pytest.raises(RuntimeError, match="the block's own"), block as ws:
        raised = ws.id
        raise RuntimeError("the block's own")
    state = client.get_workspace(workspace=raised).workspace.state
    assert state == WorkspaceState.WORKSPACE_STATE_RELEASED


def test_attach(client: reef.Client, repo: str) -> None:
    from datetime import timedelta

    created = client.create_workspace(
        repo=repo,
        purpose="attach me",
        owner="attach-test@host",
        lease_length=timedelta(seconds=3),
    ).workspace
    with client.attach(created.id, owner=created.owner, report_presence=False) as ws:
        time.sleep(4)
        assert not ws.lease_lost
        assert (
            client.get_workspace(workspace=ws.id).workspace.state
            == WorkspaceState.WORKSPACE_STATE_ACTIVE
        )
    assert (
        client.get_workspace(workspace=created.id).workspace.state
        == WorkspaceState.WORKSPACE_STATE_ACTIVE
    )
    with client.attach(created.id, owner=created.owner, release=True, report_presence=False):
        pass
    assert (
        client.get_workspace(workspace=created.id).workspace.state
        == WorkspaceState.WORKSPACE_STATE_RELEASED
    )
    with pytest.raises(reef.ConflictError), client.attach(created.id, owner=created.owner):
        pass


def test_files_without_a_workspace(client: reef.Client, repo: str) -> None:
    edited = client.edit_files(
        repo=repo, new_on="main", edits=[FileEdit(path="api.txt", content=b"by the API\n")]
    )
    read = client.read_file(repo=repo, revision=edited.change.change_id, path="api.txt")
    assert read.content == b"by the API\n"


def test_artifact_reads_back_through_the_stream(client: reef.Client, repo: str) -> None:
    # R3.02: an upload in two chunks, then ReadArtifact's server stream.
    import hashlib

    data = bytes(range(256)) * 6000
    sha = hashlib.sha256(data).hexdigest()
    chunk = 1 << 20
    probe = client.upload_artifact(repo=repo, sha256=sha, size=len(data), offset=0, data=b"")
    assert probe.received == 0
    first = client.upload_artifact(
        repo=repo, sha256=sha, size=len(data), offset=0, data=data[:chunk]
    )
    assert first.received == chunk
    last = client.upload_artifact(
        repo=repo, sha256=sha, size=len(data), offset=chunk, data=data[chunk:]
    )
    assert last.created
    frames = list(client.read_artifact(repo=repo, sha256=sha[:12]))
    assert frames[0].artifact.size == len(data)
    assert b"".join(frame.data for frame in frames) == data


def test_subscription_streams_and_cancels(client: reef.Client, repo: str) -> None:
    with client.workspace(repo, purpose="stream", report_presence=False) as ws:
        stream = client.subscribe_operations(repos=[repo])
        got: list = []

        def read() -> None:
            for frame in stream:
                got.extend(frame.operations)

        reader = threading.Thread(target=read, daemon=True)
        reader.start()
        time.sleep(0.5)
        (ws.path / "s.txt").write_text("x")
        snapshot = ws.snapshot()
        wait_for(lambda: any(op.id == snapshot.operation.id for op in got), message="the operation")
        stream.cancel()
        reader.join(5)
        assert not reader.is_alive(), "a cancelled stream ends"
        assert list(stream) == []


def test_presence_reporter(client: reef.Client, repo: str) -> None:
    with client.workspace(repo, purpose="editing", report_presence=False) as ws:
        reporter = Reporter(
            client,
            Subject(agent="sdk-test", kind=PresenceKind.PRESENCE_KIND_EDITING, workspace=ws.id),
        )
        reporter.set(State(paths=["a.txt"]))

        def entries() -> list:
            stream = client.subscribe_presence(workspaces=[ws.id])
            try:
                for frame in stream:
                    if frame.synced:
                        return [e.presence for e in frame.events if not e.ended]
            finally:
                stream.cancel()
            return []

        wait_for(
            lambda: any(list(p.paths) == ["a.txt"] for p in entries()), message="the editing entry"
        )
        assert reporter.is_live()
        reporter.clear()
        wait_for(
            lambda: not any(p.kind == PresenceKind.PRESENCE_KIND_EDITING for p in entries()),
            message="the cleared entry",
        )


async def test_async_client(daemon: Daemon, repo: str) -> None:
    client = await reef.aio.connect(state_dir=daemon.state, agent="sdk-async")
    async with client:
        status = await client.daemon_status()
        assert status.api_revision == reef.API_REVISION
        with pytest.raises(reef.NotFoundError):
            await client.get_workspace(workspace="abcd0123")
        ids = [op.id async for op in client.list_operations_all(repo=repo, page_size=2)]
        assert len(ids) == len(set(ids)) > 0

        async with client.workspace(repo, purpose="async block") as ws:
            (ws.path / "async.txt").write_text("async\n")
            async with client.subscribe_presence(workspaces=[ws.id]) as stream:
                async for frame in stream:
                    if any(e.presence.activity == "async block" for e in frame.events):
                        break
            ws.activity("later")
            change = ws.change_id
        read = await client.read_file(repo=repo, revision=change, path="async.txt")
        assert read.content == b"async\n"

        async with client.workspace(repo, purpose="async stream", report_presence=False) as ws:
            stream = client.subscribe_operations(repos=[repo])
            got: list = []

            async def read() -> None:
                async for frame in stream:
                    got.extend(op.id for op in frame.operations)

            task = asyncio.create_task(read())
            await asyncio.sleep(0.3)
            (ws.path / "s.txt").write_text("y")
            snapshot = await ws.snapshot()
            for _ in range(100):
                if snapshot.operation.id in got:
                    break
                await asyncio.sleep(0.05)
            assert snapshot.operation.id in got
            stream.cancel()
            await asyncio.wait_for(task, 5)
            assert [frame async for frame in stream] == []


async def test_async_client_belongs_to_its_loop(daemon: Daemon) -> None:
    client = await reef.aio.connect(state_dir=daemon.state)
    try:
        result: list[BaseException] = []

        def other_loop() -> None:
            try:
                asyncio.run(client.get_daemon_status())
            except RuntimeError as error:
                result.append(error)

        thread = threading.Thread(target=other_loop)
        thread.start()
        thread.join(10)
        assert result and "another event loop" in str(result[0])
    finally:
        await client.close()
