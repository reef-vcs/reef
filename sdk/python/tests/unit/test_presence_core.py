"""Ports of reef_client::presence's tests (R2.11's behaviors 1 to 7) against the reporter's
core, driven the way the Rust reporter's task drives it, on a simulated clock."""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, field

from reef import _identity
from reef._presence_core import (
    BURST,
    BURST_TTL,
    CALL_DEADLINE,
    CLEAR_DEADLINE,
    REFRESH,
    REFRESH_EVERY,
    REFRESH_TTL,
    Cadence,
    Close,
    Core,
    Send,
    State,
    Subject,
    Wait,
)
from reef.errors import ErrorKind

ACTIVITY = 1


@dataclass
class Fail:
    kind: ErrorKind
    reason: str = ""


HANG = "hang"
OK = "ok"


@dataclass
class Fake:
    """Answers reports from a script, then with success; gives each new entry the next id."""

    answers: deque = field(default_factory=deque)
    reports: list = field(default_factory=list)  # (ms, request)
    clears: list = field(default_factory=list)
    ids: int = 0

    def next_answer(self) -> object:
        return self.answers.popleft() if self.answers else OK


class Sim:
    """The reporter's loop on a simulated clock: what reef_client::presence::run does."""

    def __init__(self, fake: Fake, cadence: Cadence) -> None:
        self.fake = fake
        self.core = Core(Subject(agent="bot-1", kind=ACTIVITY, workspace="ws"), cadence)
        self.now = 0.0
        self.inflight_until: float | None = None
        self.inflight_replacing = False
        self.done = False

    def set(self, activity: str) -> None:
        self.core.set(State(activity=activity))

    def settle(self) -> None:
        self.advance(0)

    def advance(self, seconds: float) -> None:
        target = round(self.now + seconds, 6)
        while not self.done:
            if self.inflight_until is not None:
                if self.inflight_until > target:
                    break
                self.now = self.inflight_until
                self.inflight_until = None
                self.core.failed(self.inflight_replacing, None, "")  # timed out
                continue
            action = self.core.next(self.now)
            if isinstance(action, Wait):
                if action.until is not None and action.until <= target:
                    self.now = action.until
                    continue
                break
            if isinstance(action, Close):
                if action.presence_id is not None:
                    self.fake.clears.append(action.presence_id)
                self.done = True
                break
            assert isinstance(action, Send)
            self.fake.reports.append((round(self.now * 1000), action.request))
            answer = self.fake.next_answer()
            if answer == HANG:
                self.inflight_until = self.now + CALL_DEADLINE
                self.inflight_replacing = action.replacing
                continue
            if isinstance(answer, Fail):
                self.core.failed(action.replacing, answer.kind, answer.reason)
                if self.core.stopped:
                    self.done = True
                continue
            if action.request.presence_id:
                presence_id = action.request.presence_id
            else:
                self.fake.ids += 1
                presence_id = f"{self.fake.ids:016x}"
            self.core.answered(self.now, presence_id)
        self.now = max(self.now, target)

    def clear(self) -> float:
        """Closes, as Reporter.clear does; how long it waited, bounded by CLEAR_DEADLINE."""
        self.core.close()
        start = self.now
        if self.inflight_until is not None:
            # The driver waits for the report in flight, at most CLEAR_DEADLINE, then cancels.
            if self.inflight_until - self.now > CLEAR_DEADLINE:
                self.done = True
                return CLEAR_DEADLINE
            self.now = self.inflight_until
            self.inflight_until = None
            self.core.failed(self.inflight_replacing, None, "")
        if not self.done:
            action = self.core.next(self.now)
            assert isinstance(action, Close)
            if action.presence_id is not None:
                self.fake.clears.append(action.presence_id)
                if self.fake.next_answer() == HANG:
                    self.done = True
                    return CLEAR_DEADLINE
            self.done = True
        return self.now - start

    def is_live(self) -> bool:
        return self.core.is_live(self.now)


def ttl_seconds(request: object) -> float:
    return request.ttl.seconds + request.ttl.nanos / 1e9  # type: ignore[attr-defined]


# 1
def test_refresh_reports_replaces_and_clears() -> None:
    fake = Fake()
    sim = Sim(fake, REFRESH)
    sim.advance(5)
    assert fake.reports == [], "nothing before set"
    sim.set("running make")
    sim.settle()
    sim.advance(10)
    sim.advance(10)
    assert len(fake.reports) == 3
    at, first = fake.reports[0]
    assert at == 5_000
    assert first.presence_id == ""
    assert (first.agent, first.workspace, first.kind) == ("bot-1", "ws", ACTIVITY)
    assert first.activity == "running make"
    assert ttl_seconds(first) == REFRESH_TTL
    for expected, (when, replacement) in zip([15_000, 25_000], fake.reports[1:], strict=True):
        assert when == expected
        assert replacement.presence_id == "0000000000000001"
        assert replacement.activity == "running make"
    assert sim.is_live()
    sim.clear()
    assert fake.clears == ["0000000000000001"]
    sim.advance(60)
    assert len(fake.reports) == 3, "nothing after clear"


# 2
def test_rereports_after_not_found() -> None:
    fake = Fake(deque([OK, Fail(ErrorKind.NOT_FOUND, "PRESENCE_NOT_FOUND")]))
    sim = Sim(fake, REFRESH)
    sim.set("a")
    sim.settle()
    sim.advance(REFRESH_EVERY)
    reports = fake.reports
    assert len(reports) == 3
    assert reports[1][1].presence_id == "0000000000000001"
    assert reports[2][0] == reports[1][0], "reported anew at once"
    assert reports[2][1].presence_id == ""
    assert reports[2][1].activity == "a"
    sim.advance(REFRESH_EVERY)
    assert fake.reports[3][1].presence_id == "0000000000000002"
    sim.clear()
    assert fake.clears == ["0000000000000002"]


# 3
def test_coalesces_within_a_second() -> None:
    fake = Fake()
    sim = Sim(fake, REFRESH)
    for n in range(10):
        sim.set(f"step {n}")
        sim.settle()
        sim.advance(0.030)
    sim.advance(1)
    reports = fake.reports
    assert len(reports) == 2
    assert (reports[0][0], reports[0][1].activity) == (0, "step 0")
    assert (reports[1][0], reports[1][1].activity) == (1_000, "step 9")
    sim.advance(2)
    sim.set("later")
    sim.settle()
    assert len(fake.reports) == 3
    assert (fake.reports[2][0], fake.reports[2][1].activity) == (3_300, "later")


# 4
def test_failures_do_not_stop_it() -> None:
    fake = Fake(
        deque(
            [
                Fail(ErrorKind.UNAVAILABLE),
                Fail(ErrorKind.UNAVAILABLE, "TOO_MANY_PRESENCE_ENTRIES"),
                OK,
            ]
        )
    )
    sim = Sim(fake, REFRESH)
    sim.set("a")
    sim.settle()
    sim.advance(REFRESH_EVERY)
    sim.advance(REFRESH_EVERY)
    assert len(fake.reports) == 3, "retried at each refresh"
    assert all(request.presence_id == "" for _, request in fake.reports)
    assert sim.is_live()

    for failure in (
        Fail(ErrorKind.CONFLICT),
        Fail(ErrorKind.NOT_FOUND, "WORKSPACE_NOT_FOUND"),
        Fail(ErrorKind.UNSUPPORTED),
    ):
        fake = Fake(deque([failure]))
        sim = Sim(fake, REFRESH)
        sim.set("a")
        sim.settle()
        sim.advance(REFRESH_EVERY * 3)
        sim.set("b")
        sim.settle()
        assert len(fake.reports) == 1, "stopped for good"
        assert not sim.is_live()
        sim.clear()
        assert fake.clears == []


# 4, with an entry held
def test_failures_with_an_entry_held() -> None:
    fake = Fake(deque([OK, Fail(ErrorKind.UNAVAILABLE), OK, Fail(ErrorKind.CONFLICT)]))
    sim = Sim(fake, REFRESH)
    sim.set("a")
    sim.settle()
    for _ in range(3):
        sim.advance(REFRESH_EVERY)
    sim.advance(REFRESH_EVERY * 3)
    assert len(fake.reports) == 4
    for _, replacement in fake.reports[1:]:
        assert replacement.presence_id == "0000000000000001"
    assert not sim.is_live()
    sim.clear()
    assert fake.clears == [], "the daemon ended it"


# 4, bursts
def test_burst_drops_a_refused_report() -> None:
    fake = Fake(deque([Fail(ErrorKind.UNAVAILABLE, "TOO_MANY_PRESENCE_ENTRIES")]))
    sim = Sim(fake, BURST)
    sim.set("get_status")
    sim.settle()
    sim.advance(30)
    assert len(fake.reports) == 1
    sim.set("list_files")
    sim.settle()
    assert len(fake.reports) == 2
    assert fake.reports[1][1].presence_id == ""
    assert fake.reports[1][1].activity == "list_files"


# 5
def test_burst_does_not_refresh() -> None:
    fake = Fake()
    sim = Sim(fake, BURST)
    sim.set("get_status")
    sim.settle()
    sim.advance(5)
    sim.set("list_files")
    sim.settle()
    sim.advance(50)
    assert len(fake.reports) == 2
    assert ttl_seconds(fake.reports[0][1]) == BURST_TTL
    assert fake.reports[1][1].presence_id == "0000000000000001"
    assert sim.is_live()
    sim.advance(11)
    assert not sim.is_live(), "60 s after the last report"
    assert len(fake.reports) == 2


# 6
def test_clear_is_bounded() -> None:
    fake = Fake(deque([OK, HANG]))
    sim = Sim(fake, REFRESH)
    sim.set("a")
    sim.settle()
    assert sim.clear() <= CLEAR_DEADLINE
    assert fake.clears == ["0000000000000001"]

    # A report that hangs is not waited for past the deadline either.
    fake = Fake(deque([HANG]))
    sim = Sim(fake, REFRESH)
    sim.set("a")
    sim.settle()
    assert sim.clear() <= CLEAR_DEADLINE

    fake = Fake()
    sim = Sim(fake, REFRESH)
    sim.clear()
    assert fake.reports == [], "nothing before set"
    assert fake.clears == [], "no entry held"

    # A set the loop has not yet sent is dropped by a clear.
    fake = Fake()
    sim = Sim(fake, REFRESH)
    sim.set("a")
    sim.clear()
    assert fake.reports == [], "never sent"
    assert fake.clears == [], "no entry held"


# 7
def test_agent_rule() -> None:
    agent = _identity.presence_agent
    assert agent("bot", "u@h:1", "u@h") == "bot"
    assert agent(None, "u@h:1", "u@h") == "u@h:1"
    assert agent("  ", "u@h:1", "u@h") == "u@h:1"
    assert agent(None, None, "u@h") == "u@h"
    assert agent("", "", "u@h") == "u@h"
    assert agent(None, "équipe", "u@h") == "u@h", "not ASCII"
    assert agent(None, "x" * 300, "u@h") == "u@h", "too long"


def test_set_after_close_is_ignored() -> None:
    core = Core(Subject(agent="a", kind=ACTIVITY), REFRESH)
    core.close()
    assert not core.set(State(activity="x"))
    assert isinstance(core.next(0), Close)
