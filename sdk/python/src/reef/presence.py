"""Reporting presence (R2.11's contract) from a sync program.

A :class:`Reporter` holds one entry for one subject: nothing is sent until the first
:meth:`Reporter.set`; it replaces the entry at most once a second, refreshes it while the activity
lasts, and clears it within two seconds at :meth:`Reporter.clear`. Presence never fails the work:
failures are logged at ``debug`` on the ``reef`` logger and retried.
"""

from __future__ import annotations

import logging
import threading
import time
from collections.abc import Sequence
from typing import TYPE_CHECKING, Any

import grpc

from reef import _rpcs
from reef._presence_core import (
    BURST,
    CALL_DEADLINE,
    CLEAR_DEADLINE,
    REFRESH,
    Cadence,
    Close,
    Core,
    Send,
    State,
    Subject,
    Wait,
)
from reef.errors import from_rpc_error

if TYPE_CHECKING:
    from reef.client import Client

log = logging.getLogger("reef")

__all__ = ["BURST", "REFRESH", "Cadence", "Reporter", "State", "Subject"]


class Reporter:
    """Holds one presence entry for one subject, from a daemon thread.

    Args:
        client: The client to report through.
        subject: The entry's agent, kind and scope, which replacements keep.
        cadence: ``REFRESH`` (an activity that lasts) or ``BURST`` (activity in bursts).
    """

    def __init__(
        self,
        client: Client,
        subject: Subject,
        cadence: Cadence = REFRESH,
        *,
        clear_deadline: float = CLEAR_DEADLINE,
        call_deadline: float = CALL_DEADLINE,
    ) -> None:
        self._client = client
        self._core = Core(subject, cadence)
        self._cond = threading.Condition()
        self._clear_deadline = clear_deadline
        self._call_deadline = call_deadline
        self._inflight: Any = None
        self._thread = threading.Thread(target=self._guarded, name="reef-presence", daemon=True)
        self._thread.start()

    def set(
        self,
        state: State | None = None,
        *,
        activity: str = "",
        change: str = "",
        paths: Sequence[str] = (),
        progress: Any = None,
    ) -> None:
        """What the entry says from now on: sent at once if the last report was a second ago or
        more, else at that second's end with whatever was set last.
        """
        if state is None:
            state = State(activity=activity, change=change, paths=tuple(paths), progress=progress)
        with self._cond:
            if self._core.set(state):
                self._cond.notify()

    def is_live(self) -> bool:
        """Whether it may still hold a live entry."""
        with self._cond:
            return self._core.is_live(time.monotonic())

    def clear(self) -> None:
        """Stops reporting and clears the entry, waiting at most two seconds in all."""
        with self._cond:
            self._core.close()
            self._cond.notify()
        self._thread.join(self._clear_deadline)
        if self._thread.is_alive():
            inflight = self._inflight
            if inflight is not None:
                inflight.cancel()

    def _call(self, rpc: Any, request: Any) -> Any:
        call = self._client._transport.callable(rpc)
        future = call.future(
            request, timeout=self._call_deadline, metadata=self._client._metadata()
        )
        self._inflight = future
        try:
            return future.result()
        finally:
            self._inflight = None

    def _guarded(self) -> None:
        # Presence never fails the work, nor prints a traceback from its thread: a channel
        # closed under it (the client closed inside a block) ends reporting quietly.
        try:
            self._run()
        except Exception:
            log.debug("presence reporting stopped", exc_info=True)

    def _run(self) -> None:
        while True:
            with self._cond:
                action = self._core.next(time.monotonic())
                if isinstance(action, Wait):
                    timeout = None if action.until is None else action.until - time.monotonic()
                    if timeout is None or timeout > 0:
                        self._cond.wait(timeout)
                    continue
            if isinstance(action, Close):
                if action.presence_id is not None:
                    from reef.api.v1 import ClearPresenceRequest

                    try:
                        self._call(
                            _rpcs.CLEAR_PRESENCE,
                            ClearPresenceRequest(presence_id=action.presence_id),
                        )
                    except grpc.RpcError as error:
                        log.debug("presence clear failed: %s", error.details())
                    except grpc.FutureCancelledError:
                        log.debug("presence clear cancelled at its deadline")
                return
            assert isinstance(action, Send)
            try:
                response = self._call(_rpcs.REPORT_PRESENCE, action.request)
            except grpc.FutureCancelledError:
                # clear() cancelled it at its deadline: no answer, as for a timeout.
                with self._cond:
                    self._core.failed(action.replacing, None, "")
                continue
            except grpc.RpcError as error:
                if error.code() in (grpc.StatusCode.DEADLINE_EXCEEDED, grpc.StatusCode.CANCELLED):
                    log.debug("presence report timed out")
                    with self._cond:
                        self._core.failed(action.replacing, None, "")
                    continue
                failure = from_rpc_error(error, _rpcs.REPORT_PRESENCE.path)
                log.debug("presence report failed: %s", failure.message)
                with self._cond:
                    self._core.failed(action.replacing, failure.kind, failure.reason)
                    if self._core.stopped:
                        log.debug("presence reporting stopped")
                        return
                continue
            with self._cond:
                self._core.answered(time.monotonic(), response.presence.id)
