"""Reporting presence (R2.11's contract) from asyncio: :class:`reef.presence.Reporter` as a task."""

from __future__ import annotations

import asyncio
import contextlib
import logging
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
    from reef.aio.client import AsyncClient

log = logging.getLogger("reef")

__all__ = ["BURST", "REFRESH", "Cadence", "Reporter", "State", "Subject"]


class Reporter:
    """Holds one presence entry for one subject, from a task on the running loop.

    Args:
        client: The client to report through.
        subject: The entry's agent, kind and scope, which replacements keep.
        cadence: ``REFRESH`` (an activity that lasts) or ``BURST`` (activity in bursts).
    """

    def __init__(
        self,
        client: AsyncClient,
        subject: Subject,
        cadence: Cadence = REFRESH,
        *,
        clear_deadline: float = CLEAR_DEADLINE,
        call_deadline: float = CALL_DEADLINE,
    ) -> None:
        self._client = client
        self._core = Core(subject, cadence)
        self._wake = asyncio.Event()
        self._clear_deadline = clear_deadline
        self._call_deadline = call_deadline
        self._loop = asyncio.get_running_loop()
        self._task = asyncio.create_task(self._guarded(), name="reef-presence")

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
        if self._core.set(state):
            self._wake.set()

    def is_live(self) -> bool:
        """Whether it may still hold a live entry."""
        return self._core.is_live(self._loop.time())

    async def clear(self) -> None:
        """Stops reporting and clears the entry, waiting at most two seconds in all."""
        self._core.close()
        self._wake.set()
        try:
            await asyncio.wait_for(asyncio.shield(self._task), self._clear_deadline)
        except TimeoutError:
            self._task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._task

    async def _call(self, rpc: Any, request: Any) -> Any:
        call = self._client._transport.callable(rpc)
        return await call(request, timeout=self._call_deadline, metadata=self._client._metadata())

    async def _guarded(self) -> None:
        try:
            await self._run()
        except Exception:
            # Presence never fails the work: a channel closed under it ends reporting quietly.
            log.debug("presence reporting stopped", exc_info=True)

    async def _run(self) -> None:
        while True:
            action = self._core.next(self._loop.time())
            if isinstance(action, Wait):
                self._wake.clear()
                timeout = None if action.until is None else action.until - self._loop.time()
                if timeout is None or timeout > 0:
                    with contextlib.suppress(TimeoutError):
                        await asyncio.wait_for(self._wake.wait(), timeout)
                continue
            if isinstance(action, Close):
                if action.presence_id is not None:
                    from reef.api.v1 import ClearPresenceRequest

                    try:
                        await self._call(
                            _rpcs.CLEAR_PRESENCE,
                            ClearPresenceRequest(presence_id=action.presence_id),
                        )
                    except grpc.RpcError as error:
                        log.debug("presence clear failed: %s", error.details())
                return
            assert isinstance(action, Send)
            try:
                response = await self._call(_rpcs.REPORT_PRESENCE, action.request)
            except grpc.RpcError as error:
                if error.code() == grpc.StatusCode.DEADLINE_EXCEEDED:
                    log.debug("presence report timed out")
                    self._core.failed(action.replacing, None, "")
                    continue
                failure = from_rpc_error(error, _rpcs.REPORT_PRESENCE.path)
                log.debug("presence report failed: %s", failure.message)
                self._core.failed(action.replacing, failure.kind, failure.reason)
                if self._core.stopped:
                    log.debug("presence reporting stopped")
                    return
                continue
            self._core.answered(self._loop.time(), response.presence.id)
