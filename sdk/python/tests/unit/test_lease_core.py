"""The lease heartbeater's schedule and notes, as ``reef ws run``'s (R1.21)."""

from __future__ import annotations

import datetime as dt

import pytest

from reef._lease_core import LeaseCore, lease_gone
from reef.api.v1 import Lease
from reef.errors import ErrorKind

NOW = dt.datetime(2026, 11, 2, 12, 0, tzinfo=dt.UTC)


def lease(every: float = 20, expires_in: float = 60, grace: float = 300) -> Lease:
    message = Lease(id="l1")
    message.heartbeat_every.FromTimedelta(dt.timedelta(seconds=every))
    message.grace.FromTimedelta(dt.timedelta(seconds=grace))
    message.expires_at.FromDatetime(NOW + dt.timedelta(seconds=expires_in))
    return message


def test_lease_core() -> None:
    core = LeaseCore("ws1", lease())
    assert core.next_delay() == 20

    # Backoff while unreachable: 1, 2, 4, 8, 16, 30, 30; one note when the link drops.
    notes = core.unreachable("refused", NOW)
    assert len(notes) == 1 and "unreachable (refused)" in notes[0]
    delays = [core.next_delay()]
    for _ in range(6):
        assert core.unreachable("refused", NOW) == []
        delays.append(core.next_delay())
    assert delays == [1, 2, 4, 8, 16, 30, 30]

    # Past expires_at + grace: one note that the lease ran out.
    late = NOW + dt.timedelta(seconds=361)
    notes = core.unreachable("refused", late)
    assert len(notes) == 1 and "has expired" in notes[0]
    assert core.unreachable("refused", late) == []

    # Back: one note, the daemon's cadence, the lapse moved.
    notes = core.renewed(lease(every=10, expires_in=600))
    assert notes == ["reef: daemon reachable again; lease renewed"]
    assert core.next_delay() == 10
    assert core.renewed(lease()) == []


def test_a_lease_needs_its_cadence() -> None:
    with pytest.raises(ValueError, match="heartbeat_every"):
        LeaseCore("ws1", Lease(id="l1"))


def test_lease_gone() -> None:
    assert lease_gone(ErrorKind.CONFLICT)
    assert lease_gone(ErrorKind.NOT_FOUND)
    assert not lease_gone(ErrorKind.INTERNAL)
    assert not lease_gone(ErrorKind.UNAVAILABLE)


def test_an_answer_without_a_lease_changes_nothing() -> None:
    core = LeaseCore("ws1", lease())
    core.unreachable("refused", NOW)
    assert core.renewed(None) == []
    assert not core.up, "as heartbeat.rs: only an answer with a lease marks the link up"
