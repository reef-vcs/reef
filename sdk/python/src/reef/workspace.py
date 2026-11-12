"""A workspace held for a block of code, as ``reef ws run`` holds one for a command (R2.06).

``client.workspace(...)`` creates a workspace, and ``client.attach(id)`` takes over an existing
one; while the block runs, the lease is renewed on the daemon's cadence and one ``ACTIVITY``
presence entry is reported (R2.11); at the end the entry is cleared, the heartbeats stop, and the
workspace is released with a final snapshot (unless kept), so every edit lands in its change.
"""

from __future__ import annotations

import datetime as dt
import logging
import sys
import threading
from pathlib import Path
from types import TracebackType
from typing import TYPE_CHECKING, Any

from reef import _identity
from reef._lease_core import HEARTBEAT_DEADLINE, LeaseCore, lease_gone
from reef._presence_core import REFRESH, State, Subject
from reef.errors import ConflictError, ReefError

if TYPE_CHECKING:
    from reef.api.v1 import (
        DeclareProvenanceResponse,
        EditFilesResponse,
        GetDiffResponse,
        GetStatusResponse,
        MoveWorkspaceResponse,
        ReadFileResponse,
        RebaseChangeResponse,
        ReleaseWorkspaceResponse,
        SnapshotWorkspaceResponse,
        Workspace,
    )
    from reef.client import Client
    from reef.presence import Reporter
    from reef.provenance import CreatePlan

log = logging.getLogger("reef")

PURPOSE_MAX_CHARS = 200
PRESENCE_KIND_ACTIVITY = 1


def command_activity() -> str:
    """``running <argv>``, cut as ``ws run`` cuts its purpose."""
    return "running " + " ".join(sys.argv)[:PURPOSE_MAX_CHARS]  # ws run's purpose_of


def check_attachable(workspace: Workspace, owner: str | None) -> None:
    """``ws run --workspace``'s rule: the workspace must be active with a live lease, and held
    by ``owner`` or, without one, by this user on this machine.

    Raises:
        ConflictError: It may not be attached.
    """
    from reef.api.v1 import LeaseState, WorkspaceState

    live = workspace.state == WorkspaceState.WORKSPACE_STATE_ACTIVE and (
        workspace.HasField("lease")
        and workspace.lease.state
        in (LeaseState.LEASE_STATE_ACTIVE, LeaseState.LEASE_STATE_IN_GRACE)
    )
    if not live:
        raise ConflictError(
            f"workspace {workspace.id} is {WorkspaceState.Name(workspace.state)} and its lease has "
            "ended; only an active workspace can be attached",
            hint="Create a new workspace instead.",
        )
    if not may_attach(workspace.owner, owner, _identity.default_owner()):
        raise ConflictError(
            f"workspace {workspace.id} is held by {workspace.owner}; pass "
            f"owner={workspace.owner!r} to attach to it"
        )


def may_attach(holder: str, owner: str | None, me: str) -> bool:
    """Whether a workspace held by ``holder`` may be attached for ``owner``, or for ``me``."""
    if owner is not None:
        return holder == owner
    rest = holder.removeprefix(me)
    return rest != holder and (rest == "" or rest.startswith(":"))


class Heartbeater:
    """Renews one lease from a daemon thread until stopped or the lease ends."""

    def __init__(self, client: Client, workspace: Workspace) -> None:
        self._client = client
        self._core = LeaseCore(workspace.id, workspace.lease)
        self._stop = threading.Event()
        self.lost = False
        self._thread = threading.Thread(target=self._guarded, name="reef-heartbeat", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Stops the heartbeats."""
        self._stop.set()
        self._thread.join(HEARTBEAT_DEADLINE + 1)

    def _guarded(self) -> None:
        # Heartbeats never raise into the block, nor print a traceback from their thread: a
        # channel closed under them (the client closed inside the block) stops them quietly.
        try:
            self._run()
        except Exception:
            log.debug("heartbeats stopped", exc_info=True)

    def _run(self) -> None:
        while not self._stop.wait(self._core.next_delay()):
            try:
                response = self._client.with_options(timeout=HEARTBEAT_DEADLINE).renew_lease(
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


class WorkspaceHandle:
    """The workspace a block holds, with the calls an agent makes on it most.

    Attributes:
        id: The workspace's id.
        repo_id: Its repository.
        workspace: The last ``Workspace`` the daemon returned for it.
    """

    def __init__(self, client: Client, workspace: Workspace, block: WorkspaceBlock) -> None:
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

    def snapshot(self) -> SnapshotWorkspaceResponse:
        """Snapshots the workspace now."""
        response = self._client.snapshot_workspace(workspace=self.id)
        return response

    def status(self, **fields: Any) -> GetStatusResponse:
        """``get_status`` for the workspace."""
        return self._client.get_status(workspace=self.id, **fields)

    def diff(self, **fields: Any) -> GetDiffResponse:
        """``get_diff`` for the workspace's change."""
        return self._client.get_diff(workspace=self.id, **fields)

    def read_file(self, path: str, **fields: Any) -> ReadFileResponse:
        """``read_file`` in the workspace."""
        return self._client.read_file(workspace=self.id, path=path, **fields)

    def edit_files(self, **fields: Any) -> EditFilesResponse:
        """``edit_files`` in the workspace: its change (``@``) unless ``revision`` or ``new_on``
        names another target.
        """
        if "revision" not in fields and "new_on" not in fields:
            fields["revision"] = "@"
        return self._client.edit_files(workspace=self.id, **fields)

    def move(self, **fields: Any) -> MoveWorkspaceResponse:
        """``move_workspace``: puts the workspace on another change."""
        response = self._client.move_workspace(workspace=self.id, **fields)
        if response.HasField("workspace"):
            self._update(response.workspace)
        return response

    def rebase(self, **fields: Any) -> RebaseChangeResponse:
        """``rebase_change`` for the workspace's change (``@`` unless ``revision`` names
        another): fetches trunk and brings the change, with those above it, onto it (#259).
        A conflict is written and returned in ``conflicts``. The handle follows its change.
        """
        fields.setdefault("revision", "@")
        response = self._client.rebase_change(workspace=self.id, **fields)
        if fields["revision"] == "@" and response.HasField("change"):
            self.workspace.change_id = response.change.change_id
            self.workspace.commit_id = response.change.commit_id
        return response

    def declare(
        self,
        *,
        model: str | None = None,
        harness: str | None = None,
        task: str | None = None,
        instruction: str | bytes | None = None,
        instruction_sha256: str | None = None,
        client_session: str | None = None,
        snapshot: bool = True,
    ) -> DeclareProvenanceResponse:
        """Replaces fields of the workspace's provenance context (R3.44): ``None`` leaves a
        field, ``""`` clears it. ``instruction`` is hashed here, and only its SHA-256 is sent.
        Snapshots first, so edits already made are recorded under the context they were made
        under, unless ``snapshot`` is false.

        Raises:
            IncompatibleError: The daemon is older than API revision 18.
        """
        from reef.provenance import PROVENANCE_REVISION, declare_fields

        declaration = declare_fields(
            model=model,
            harness=harness,
            task=task,
            instruction=instruction,
            instruction_sha256=instruction_sha256,
            client_session=client_session,
        )
        self._client.require_revision(PROVENANCE_REVISION)
        return self._client.declare_provenance(
            workspace=self.id, declaration=declaration, no_snapshot=not snapshot
        )

    def activity(self, text: str) -> None:
        """Replaces the presence entry's text, at most once a second."""
        reporter = self._block.reporter
        if reporter is not None:
            reporter.set(State(activity=text))

    def release(self) -> ReleaseWorkspaceResponse:
        """Releases the workspace now, with a final snapshot; the block's end does nothing more."""
        return self._block.release_now()


class WorkspaceBlock:
    """The context manager ``client.workspace()`` and ``client.attach()`` return."""

    def __init__(
        self,
        client: Client,
        create: dict[str, Any] | None,
        *,
        attach: str | None = None,
        owner: str | None = None,
        activity: str | None,
        keep: bool,
        report_presence: bool,
        release: bool,
        provenance: CreatePlan | None = None,
    ) -> None:
        self._client = client
        self._create = create
        self._provenance = provenance
        self._attach = attach
        self._owner = owner
        self._activity = activity
        self._keep = keep
        self._report_presence = report_presence
        self._release = release and not keep
        self._released = False
        self.handle: WorkspaceHandle | None = None
        self.heartbeater: Heartbeater | None = None
        self.reporter: Reporter | None = None

    def __enter__(self) -> WorkspaceHandle:
        client = self._client
        if self._create is not None:
            fields = dict(self._create)
            fields["owner"] = fields.get("owner") or _identity.process_owner()
            workspace = self._create_workspace(fields)
        else:
            assert self._attach is not None
            workspace = client.get_workspace(workspace=self._attach).workspace
            check_attachable(workspace, self._owner)
            renewed = client.renew_lease(workspace=workspace.id, lease_id=workspace.lease.id)
            if renewed.HasField("lease"):
                workspace.lease.CopyFrom(renewed.lease)
        handle = WorkspaceHandle(client, workspace, self)
        self.handle = handle
        try:
            self.heartbeater = Heartbeater(client, workspace)
            if self._report_presence:
                self.reporter = self._start_presence(workspace)
        except BaseException:
            try:
                self._finish(None)
            except ReefError:
                log.warning("releasing workspace %s failed", workspace.id, exc_info=True)
            raise
        return handle

    def _create_workspace(self, fields: dict[str, Any]) -> Workspace:
        """Creates it with its provenance; a declaration made only of the environment's
        defaults is dropped where the daemon cannot take it, as ``reef ws create`` drops it.
        """
        from reef.provenance import PROVENANCE_DISABLED, PROVENANCE_REVISION

        client = self._client
        plan = self._provenance
        if plan is not None and plan.declaration is not None:
            if plan.explicit:
                client.require_revision(PROVENANCE_REVISION)
                fields["provenance"] = plan.declaration.message(creating=True)
            elif client.daemon_status().api_revision >= PROVENANCE_REVISION:
                fields["provenance"] = plan.declaration.message(creating=True)
        try:
            return client.create_workspace(**fields).workspace
        except ReefError as error:
            if "provenance" not in fields or plan is None or plan.explicit:
                raise
            if error.reason != PROVENANCE_DISABLED:
                raise
            del fields["provenance"]
            return client.create_workspace(**fields).workspace

    def _start_presence(self, workspace: Workspace) -> Reporter:
        from reef.presence import Reporter

        subject = Subject(
            agent=_identity.presence_agent(
                self._client.agent, workspace.owner, _identity.default_owner()
            ),
            kind=PRESENCE_KIND_ACTIVITY,
            workspace=workspace.id,
        )
        reporter = Reporter(self._client, subject, REFRESH)
        text = self._activity or workspace.purpose or command_activity()
        reporter.set(State(activity=text))
        return reporter

    def release_now(self) -> ReleaseWorkspaceResponse:
        """Stops the heartbeats and presence, and releases the workspace."""
        self._stop()
        assert self.handle is not None
        response = self._client.release_workspace(workspace=self.handle.id)
        self._released = True
        return response

    def _stop(self) -> None:
        if self.reporter is not None:
            self.reporter.clear()
            self.reporter = None
        if self.heartbeater is not None:
            self.heartbeater.stop()

    def _finish(self, exc: BaseException | None) -> None:
        self._stop()
        if not self._release or self._released or self.handle is None:
            return
        try:
            self._client.release_workspace(workspace=self.handle.id)
            self._released = True
        except ReefError:
            if exc is None:
                raise
            log.warning("releasing workspace %s failed", self.handle.id, exc_info=True)

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self._finish(exc)
