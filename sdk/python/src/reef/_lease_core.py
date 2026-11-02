"""Keeping a lease alive, as ``reef ws run``'s heartbeater does (R1.21), with no I/O.

Renews on the daemon's ``heartbeat_every``; while the daemon is unreachable, retries after 1, 2,
4, 8, 16, then 30 s, noting each change of the link once, and once that the lease ran out; stops
when the daemon says the lease ended. The drivers log the notes at ``warning``.
"""

from __future__ import annotations

import datetime as dt
from typing import Any

from reef.errors import ErrorKind

HEARTBEAT_DEADLINE = 5.0
"""Each heartbeat's deadline."""
BACKOFF_CAP = 30.0
"""The longest wait between heartbeats while the daemon is unreachable."""


def lease_gone(kind: ErrorKind) -> bool:
    """Whether a heartbeat failed because the lease can no longer be renewed."""
    return kind in (ErrorKind.CONFLICT, ErrorKind.NOT_FOUND)


def _seconds(duration: Any) -> float:
    return float(duration.seconds) + float(duration.nanos) / 1e9


def _lapse(lease: Any, grace: float) -> dt.datetime | None:
    if not lease.HasField("expires_at"):
        return None
    expires: dt.datetime = lease.expires_at.ToDatetime(tzinfo=dt.UTC)
    return expires + dt.timedelta(seconds=grace)


class LeaseCore:
    """One lease's heartbeat schedule and what to say about it."""

    def __init__(self, workspace: str, lease: Any) -> None:
        """For ``lease`` (a ``reef.api.v1.Lease``) on ``workspace``.

        Raises:
            ValueError: The lease has no ``heartbeat_every``.
        """
        every = _seconds(lease.heartbeat_every) if lease.HasField("heartbeat_every") else 0.0
        if every <= 0:
            raise ValueError("the daemon's lease has no heartbeat_every")
        self.workspace = workspace
        self.lease_id: str = lease.id
        self.every = every
        self.grace = _seconds(lease.grace) if lease.HasField("grace") else 0.0
        self.lapses_at = _lapse(lease, self.grace)
        self.warned_lapsed = False
        self.failures = 0
        self.up = True

    def next_delay(self) -> float:
        """How long to wait before the next heartbeat."""
        if self.up:
            return self.every
        doubling = min(max(self.failures - 1, 0), 5)
        return min(float(1 << doubling), BACKOFF_CAP)

    def renewed(self, lease: Any) -> list[str]:
        """A heartbeat answered with ``lease``; the notes to log."""
        if lease is None:
            return []  # as heartbeat.rs: an answer without a lease changes nothing
        notes = []
        if not self.up:
            notes.append("reef: daemon reachable again; lease renewed")
        self.up = True
        self.failures = 0
        if lease.HasField("heartbeat_every"):
            every = _seconds(lease.heartbeat_every)
            if every > 0:
                self.every = every
        self.lapses_at = _lapse(lease, self.grace) or self.lapses_at
        self.warned_lapsed = False
        return notes

    def unreachable(self, reason: str, now: dt.datetime) -> list[str]:
        """A heartbeat failed for a reason other than the lease ending; the notes to log."""
        notes = []
        if self.up:
            notes.append(
                f"reef: daemon unreachable ({reason}); retrying, and the block keeps running"
            )
        self.up = False
        self.failures += 1
        if self.lapses_at is not None and now > self.lapses_at and not self.warned_lapsed:
            self.warned_lapsed = True
            notes.append(
                f"reef: lease {self.lease_id} has expired; the daemon may reclaim workspace "
                f"{self.workspace} and remove its directory"
            )
        return notes

    def gone(self, message: str) -> str:
        """The lease ended; the note to log."""
        return (
            f"reef: lease {self.lease_id} cannot be renewed: {message}; "
            "the workspace may be reclaimed"
        )
