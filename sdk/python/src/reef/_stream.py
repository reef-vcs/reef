"""A server stream on the sync client."""

from __future__ import annotations

from collections.abc import Callable, Iterator
from types import TracebackType
from typing import Any, Generic, TypeVar

import grpc

from reef.errors import ReefError

T = TypeVar("T")


class Stream(Generic[T]):
    """The responses of a server-streaming RPC.

    Iterate it for each response; leaving its ``with`` block, or :meth:`cancel`, ends it. A
    cancelled stream stops iterating rather than raising. The daemon's errors, mid-stream ones
    included, are raised as :class:`reef.ReefError`.
    """

    def __init__(self, call: Any, error: Callable[[grpc.RpcError], ReefError]) -> None:
        self._call = call
        self._error = error
        self._cancelled = False

    def __iter__(self) -> Iterator[T]:
        return self

    def __next__(self) -> T:
        if self._cancelled:
            raise StopIteration
        try:
            return next(self._call)  # type: ignore[no-any-return]
        except grpc.RpcError as error:
            if self._cancelled or (error.code() == grpc.StatusCode.CANCELLED and self._cancelled):
                raise StopIteration from None
            raise self._error(error) from None

    def cancel(self) -> None:
        """Ends the stream; iterating it afterwards stops at once."""
        self._cancelled = True
        self._call.cancel()

    def __enter__(self) -> Stream[T]:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.cancel()
