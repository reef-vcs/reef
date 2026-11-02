"""A workspace held for an ``async with`` block: :mod:`reef.workspace` for asyncio."""

from __future__ import annotations

import asyncio
import contextlib
import datetime as dt
import logging
from pathlib import Path
from types import TracebackType
from typing import TYPE_CHECKING, Any

from reef import _identity
from reef._lease_core import HEARTBEAT_DEADLINE, LeaseCore, lease_gone
from reef._presence_core import REFRESH, State, Subject
from reef.errors import ReefError
from reef.workspace import PRESENCE_KIND_ACTIVITY, check_attachable
from reef.workspace import command_activity as _command_activity

if TYPE_CHECKING:
    from reef.aio.client import AsyncClient
    from reef.aio.presence import Reporter
    from reef.api.v1 import (
        EditFilesResponse,
        GetDiffResponse,
        GetStatusResponse,
        MoveWorkspaceResponse,
        ReadFileResponse,
        ReleaseWorkspaceResponse,
        SnapshotWorkspaceResponse,
        Workspace,
    )

log = logging.getLogger("reef")


class AsyncHeartbeater:
    """Renews one lease from a task until stopped or the lease ends."""

    def __init__(self, client: AsyncClient, workspace: Workspace) -> None:
        self._client = client.with_options(timeout=HEARTBEAT_DEADLINE)
        self._core = LeaseCore(workspace.id, workspace.lease)
        self.lost = False
        self._task = asyncio.create_task(self._guarded(), name="reef-heartbeat")

    async def stop(self) -> None:
        """Stops the heartbeats."""
        self._task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await self._task

    async def _guarded(self) -> None:
        try:
            await self._run()
        except Exception:
            # Heartbeats never raise into the block: a channel closed under them stops them.
            log.debug("heartbeats stopped", exc_info=True)

    async def _run(self) -> None:
        while True:
            await asyncio.sleep(self._core.next_delay())
            try:
                response = await self._client.renew_lease(
                    workspace=self._core.workspace, lease_id=self._core.lease_id
                )
            except ReefError as error:
                if lease_gone(error.kind):
                    self.lost = True
                    log.warning("%s", self._core.gone(error.message))
                    return
                for note in self._core.unreachable(error.message, dt.datetime.now(dt.UTC)):
                    log.warning("%s", note)
                continue
            lease = response.lease if response.HasField("lease") else None
            for note in self._core.renewed(lease):
                log.warning("%s", note)


class AsyncWorkspaceHandle:
    """The workspace an ``async with`` block holds; see :class:`reef.WorkspaceHandle`."""

    def __init__(
        self, client: AsyncClient, workspace: Workspace, block: AsyncWorkspaceBlock
    ) -> None:
        self._client = client
        self._block = block
        self.workspace = workspace
        self.id: str = workspace.id
        self.repo_id: str = workspace.repo_id

    @property
    def path(self) -> Path:
        """The workspace's directory, on the daemon's machine."""
        return Path(self.workspace.path)

    @property
    def change_id(self) -> str:
        """Its change, as of the last call through this handle."""
        return self.workspace.change_id

    @property
    def lease_lost(self) -> bool:
        """Whether the daemon said the lease ended while the block ran."""
        heartbeater = self._block.heartbeater
        return heartbeater is not None and heartbeater.lost

    def _update(self, workspace: Workspace | None) -> None:
        if workspace is not None and workspace.id:
            self.workspace = workspace

    async def snapshot(self) -> SnapshotWorkspaceResponse:
        """Snapshots the workspace now."""
        response = await self._client.snapshot_workspace(workspace=self.id)
        return response

    async def status(self, **fields: Any) -> GetStatusResponse:
        """``get_status`` for the workspace."""
        return await self._client.get_status(workspace=self.id, **fields)

    async def diff(self, **fields: Any) -> GetDiffResponse:
        """``get_diff`` for the workspace's change."""
        return await self._client.get_diff(workspace=self.id, **fields)

    async def read_file(self, path: str, **fields: Any) -> ReadFileResponse:
        """``read_file`` in the workspace."""
        return await self._client.read_file(workspace=self.id, path=path, **fields)

    async def edit_files(self, **fields: Any) -> EditFilesResponse:
        """``edit_files`` in the workspace: its change (``@``) unless ``revision`` or ``new_on``
        names another target.
        """
        if "revision" not in fields and "new_on" not in fields:
            fields["revision"] = "@"
        return await self._client.edit_files(workspace=self.id, **fields)

    async def move(self, **fields: Any) -> MoveWorkspaceResponse:
        """``move_workspace``: puts the workspace on another change."""
        response = await self._client.move_workspace(workspace=self.id, **fields)
        if response.HasField("workspace"):
            self._update(response.workspace)
        return response

    def activity(self, text: str) -> None:
        """Replaces the presence entry's text, at most once a second."""
        reporter = self._block.reporter
        if reporter is not None:
            reporter.set(State(activity=text))

    async def release(self) -> ReleaseWorkspaceResponse:
        """Releases the workspace now, with a final snapshot; the block's end does nothing more."""
        return await self._block.release_now()


class AsyncWorkspaceBlock:
    """The async context manager ``client.workspace()`` and ``client.attach()`` return."""

    def __init__(
        self,
        client: AsyncClient,
        create: dict[str, Any] | None,
        *,
        attach: str | None = None,
        owner: str | None = None,
        activity: str | None,
        keep: bool,
        report_presence: bool,
        release: bool,
    ) -> None:
        self._client = client
        self._create = create
        self._attach = attach
        self._owner = owner
        self._activity = activity
        self._report_presence = report_presence
        self._release = release and not keep
        self._released = False
        self.handle: AsyncWorkspaceHandle | None = None
        self.heartbeater: AsyncHeartbeater | None = None
        self.reporter: Reporter | None = None

    async def __aenter__(self) -> AsyncWorkspaceHandle:
        client = self._client
        if self._create is not None:
            fields = dict(self._create)
            fields["owner"] = fields.get("owner") or _identity.process_owner()
            workspace = (await client.create_workspace(**fields)).workspace
        else:
            assert self._attach is not None
            workspace = (await client.get_workspace(workspace=self._attach)).workspace
            check_attachable(workspace, self._owner)
            renewed = await client.renew_lease(workspace=workspace.id, lease_id=workspace.lease.id)
            if renewed.HasField("lease"):
                workspace.lease.CopyFrom(renewed.lease)
        self.handle = AsyncWorkspaceHandle(client, workspace, self)
        try:
            self.heartbeater = AsyncHeartbeater(client, workspace)
            if self._report_presence:
                self.reporter = self._start_presence(workspace)
        except BaseException:
            try:
                await self._finish(None)
            except ReefError:
                log.warning("releasing workspace %s failed", workspace.id, exc_info=True)
            raise
        return self.handle

    def _start_presence(self, workspace: Workspace) -> Reporter:
        from reef.aio.presence import Reporter

        subject = Subject(
            agent=_identity.presence_agent(
                self._client.agent, workspace.owner, _identity.default_owner()
            ),
            kind=PRESENCE_KIND_ACTIVITY,
            workspace=workspace.id,
        )
        reporter = Reporter(self._client, subject, REFRESH)
        text = self._activity or workspace.purpose or _command_activity()
        reporter.set(State(activity=text))
        return reporter

    async def _stop(self) -> None:
        if self.reporter is not None:
            await self.reporter.clear()
            self.reporter = None
        if self.heartbeater is not None:
            await self.heartbeater.stop()

    async def release_now(self) -> ReleaseWorkspaceResponse:
        """Stops the heartbeats and presence, and releases the workspace."""
        await self._stop()
        assert self.handle is not None
        response = await self._client.release_workspace(workspace=self.handle.id)
        self._released = True
        return response

    async def _finish(self, exc: BaseException | None) -> None:
        await self._stop()
        if not self._release or self._released or self.handle is None:
            return
        try:
            await self._client.release_workspace(workspace=self.handle.id)
            self._released = True
        except ReefError:
            if exc is None:
                raise
            log.warning("releasing workspace %s failed", self.handle.id, exc_info=True)

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        await self._finish(exc)
