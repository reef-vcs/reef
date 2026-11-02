"""A server stream on the asyncio client."""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator, Awaitable, Callable
from types import TracebackType
from typing import Any, Generic, TypeVar

import grpc

from reef.errors import ReefError

T = TypeVar("T")


class AsyncStream(Generic[T]):
    """The responses of a server-streaming RPC, for ``async for``.

    Leaving its ``async with`` block, or :meth:`cancel`, ends it. A cancelled stream stops
    iterating rather than raising. The daemon's errors, mid-stream ones included, are raised as
    :class:`reef.ReefError`.
    """

    def __init__(self, call: Any, error: Callable[[grpc.RpcError], Awaitable[ReefError]]) -> None:
        self._call = call
        self._error = error
        self._cancelled = False
        self._iterator: AsyncIterator[T] | None = None

    def __aiter__(self) -> AsyncIterator[T]:
        return self

    async def __anext__(self) -> T:
        if self._cancelled:
            raise StopAsyncIteration
        if self._iterator is None:
            self._iterator = self._call.__aiter__()
        try:
            return await self._iterator.__anext__()
        except asyncio.CancelledError:
            if self._cancelled:
                raise StopAsyncIteration from None
            raise
        except grpc.RpcError as error:
            if self._cancelled:
                raise StopAsyncIteration from None
            raise (await self._error(error)) from None

    def cancel(self) -> None:
        """Ends the stream; iterating it afterwards stops at once."""
        self._cancelled = True
        self._call.cancel()

    async def __aenter__(self) -> AsyncStream[T]:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.cancel()
