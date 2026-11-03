"""R2.11's presence reporting contract as a state machine with no I/O.

A port of ``reef_client::presence::Reporter``'s loop: one entry for one subject, replaced by id at
most once a second, refreshed before its TTL runs out while the activity lasts, reported again
without an id when the daemon no longer knows it, and cleared at the end. The sync reporter
drives it from a thread, the asyncio one from a task; tests drive it with a fake clock. Times are
seconds on a monotonic clock.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Any

from reef.errors import ErrorKind

REFRESH_EVERY = 10.0
"""How often a long-lived reporter replaces its entry."""
REFRESH_TTL = 30.0
"""A long-lived reporter's TTL: three refreshes, the daemon's default."""
BURST_TTL = 60.0
"""A bursty reporter's TTL, sent with each report."""
MIN_INTERVAL = 1.0
"""The least time between two reports of one entry."""
CLEAR_DEADLINE = 2.0
"""How long a clear waits for the daemon in all."""
CALL_DEADLINE = 5.0
"""Each report's deadline."""

ENDS_REPORTING = frozenset(
    {
        ErrorKind.CONFLICT,
        ErrorKind.NOT_FOUND,
        ErrorKind.UNSUPPORTED,
        ErrorKind.INVALID_ARGUMENT,
        ErrorKind.PERMISSION_DENIED,
    }
)
"""Refusals after which reporting again cannot succeed: the subject is gone, the daemon does not
serve presence, or the report itself is wrong."""


@dataclass(frozen=True)
class Cadence:
    """When a reporter replaces its entry."""

    ttl: float
    """Each report's TTL."""
    every: float | None = None
    """For an activity that lasts, the refresh interval; None for bursts, never refreshed."""


REFRESH = Cadence(ttl=REFRESH_TTL, every=REFRESH_EVERY)
BURST = Cadence(ttl=BURST_TTL)


@dataclass(frozen=True)
class Subject:
    """An entry's fixed fields: a replacement must keep them."""

    agent: str
    kind: int
    """A ``PresenceKind`` value, any but ``PRESENCE_KIND_LEASE``."""
    repo: str = ""
    workspace: str = ""


@dataclass(frozen=True)
class State:
    """What an entry says: everything a replacement may change."""

    activity: str = ""
    change: str = ""
    paths: Sequence[str] = field(default_factory=tuple)
    progress: Any = None
    """A ``reef.api.v1.Progress``, for ``PRESENCE_KIND_PROGRESS``."""


@dataclass(frozen=True)
class Send:
    """Send this report now."""

    request: Any
    replacing: bool


@dataclass(frozen=True)
class Wait:
    """Wait until ``until`` (None: until woken)."""

    until: float | None


@dataclass(frozen=True)
class Close:
    """Clear the entry, if one is held, and stop."""

    presence_id: str | None


Action = Send | Wait | Close


class Core:
    """The reporter's state, and what it does next."""

    def __init__(self, subject: Subject, cadence: Cadence) -> None:
        self.subject = subject
        self.cadence = cadence
        self.state: State | None = None
        self.pending = False
        self.sending = False
        self.presence_id: str | None = None
        self.live_until: float | None = None
        self.closing = False
        self.stopped = False
        self.last: float | None = None

    def set(self, state: State) -> bool:
        """What the entry says from now on; whether the driver should wake."""
        if self.stopped or self.closing:
            return False
        self.state = state
        self.pending = True
        return True

    def close(self) -> None:
        """Stops reporting; the next action clears the entry."""
        self.closing = True

    def is_live(self, now: float) -> bool:
        """Whether it may hold a live entry: a report is due or in flight, or the last one's TTL
        has not run out.
        """
        return not self.stopped and (
            self.pending or self.sending or (self.live_until is not None and self.live_until > now)
        )

    def next(self, now: float) -> Action:
        """What to do at ``now``. A :class:`Send` marks the report in flight."""
        if self.closing:
            return Close(self.presence_id)
        if self.pending:
            action = self._due(now, MIN_INTERVAL)
        elif self.cadence.every is not None and self.state is not None:
            action = self._due(now, self.cadence.every)
        else:
            action = Wait(None)
        if isinstance(action, Wait):
            return action
        self.pending = False
        self.sending = True
        self.last = now
        return Send(self._request(), replacing=self.presence_id is not None)

    def _due(self, now: float, after: float) -> Action:
        if self.last is not None and now < self.last + after:
            return Wait(self.last + after)
        return Send(None, replacing=False)

    def answered(self, now: float, presence_id: str) -> None:
        """The report succeeded; the daemon's entry is ``presence_id``."""
        self.sending = False
        if presence_id:
            self.presence_id = presence_id
        self.live_until = now + self.cadence.ttl

    def failed(self, replacing: bool, kind: ErrorKind | None, reason: str) -> None:
        """The report failed (``kind`` None: it timed out). A lapsed entry is reported anew at
        once; a refusal that cannot change stops the reporter; anything else waits for the next
        set or refresh.
        """
        self.sending = False
        if kind is None:
            return
        if replacing and reason == "PRESENCE_NOT_FOUND":
            self.presence_id = None
            self.live_until = None
            self.pending = True
            self.last = None
            return
        if kind in ENDS_REPORTING:
            self.stopped = True
            self.presence_id = None

    def _request(self) -> Any:
        from google.protobuf.duration_pb2 import Duration

        from reef.api.v1 import ReportPresenceRequest

        state = self.state or State()
        ttl = Duration()
        ttl.FromNanoseconds(round(self.cadence.ttl * 1e9))
        request = ReportPresenceRequest(
            presence_id=self.presence_id or "",
            agent=self.subject.agent,
            repo=self.subject.repo,
            workspace=self.subject.workspace,
            change=state.change,
            kind=self.subject.kind,  # type: ignore[arg-type]
            activity=state.activity,
            paths=list(state.paths),
            ttl=ttl,
        )
        if state.progress is not None:
            request.progress.CopyFrom(state.progress)
        return request
