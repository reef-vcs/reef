"""What the generated methods call: building a request, and the clients' call primitives."""

from __future__ import annotations

import datetime as dt
from collections.abc import AsyncIterator, Iterable, Iterator, Mapping
from typing import TYPE_CHECKING, Any, TypeVar

from google.protobuf.descriptor import FieldDescriptor
from google.protobuf.duration_pb2 import Duration
from google.protobuf.message import Message
from google.protobuf.timestamp_pb2 import Timestamp

from reef._rpc import Rpc

if TYPE_CHECKING:
    from reef._stream import Stream
    from reef.aio._stream import AsyncStream

Req = TypeVar("Req", bound=Message)
Resp = TypeVar("Resp", bound=Message)


def method_name(rpc: Rpc[Any, Any]) -> str:
    """The RPC's method name on the clients."""
    out = []
    for index, char in enumerate(rpc.name):
        if char.isupper() and index:
            out.append("_")
        out.append(char.lower())
    return "".join(out)


def build(rpc: Rpc[Req, Any], request: Req | None, fields: Mapping[str, Any]) -> Req:
    """The request: ``request`` as given, or a new one with the keyword fields that were given.

    Raises:
        TypeError: Both a request and keyword fields, or a request of the wrong type.
        ValueError: An enum given by a name it does not have.
    """
    given = {name: value for name, value in fields.items() if value is not None}
    if request is not None:
        if given:
            raise TypeError(
                f"{method_name(rpc)}() takes a request or keyword fields, not both "
                f"(got a request and {', '.join(sorted(given))})"
            )
        if not isinstance(request, rpc.request):
            raise TypeError(
                f"{method_name(rpc)}() takes a {rpc.request.__name__}, "
                f"not a {type(request).__name__}"
            )
        return request
    message = rpc.request()
    descriptor = message.DESCRIPTOR
    for name, value in given.items():
        _set(message, descriptor.fields_by_name[name], value)
    return message


def _is_map(field: Any) -> bool:
    return (
        field.type == FieldDescriptor.TYPE_MESSAGE
        and field.message_type is not None
        and field.message_type.GetOptions().map_entry
    )


def _convert(field: Any, value: Any) -> Any:
    """One value as the message field takes it."""
    if field.type == FieldDescriptor.TYPE_ENUM and isinstance(value, str):
        enum = field.enum_type
        found = enum.values_by_name.get(value)
        if found is None:
            names = ", ".join(enum.values_by_name)
            raise ValueError(
                f"{field.name}: {value!r} is not one of {enum.name}'s values ({names})"
            )
        return found.number
    if field.type == FieldDescriptor.TYPE_MESSAGE:
        full = field.message_type.full_name
        if full == "google.protobuf.Timestamp" and isinstance(value, dt.datetime):
            stamp = Timestamp()
            stamp.FromDatetime(value)
            return stamp
        if full == "google.protobuf.Duration" and isinstance(value, dt.timedelta):
            duration = Duration()
            duration.FromTimedelta(value)
            return duration
    return value


def _set(message: Message, field: Any, value: Any) -> None:
    name = field.name
    if _is_map(field):
        target = getattr(message, name)
        value_field = field.message_type.fields_by_name["value"]
        for key, item in dict(value).items():
            converted = _convert(value_field, item)
            if value_field.type == FieldDescriptor.TYPE_MESSAGE:
                target[key].CopyFrom(converted)
            else:
                target[key] = converted
        return
    if field.is_repeated:
        if isinstance(value, (str, bytes)):
            raise TypeError(f"{name} takes a list, not a single {type(value).__name__}")
        getattr(message, name).extend(_convert(field, item) for item in value)
        return
    converted = _convert(field, value)
    if field.type == FieldDescriptor.TYPE_MESSAGE:
        target = getattr(message, name)
        target.SetInParent()
        target.CopyFrom(converted)
    else:
        setattr(message, name, converted)


class SyncCalls:
    """The sync client's call primitives, which the generated methods use."""

    def _unary(self, rpc: Rpc[Req, Resp], request: Req) -> Resp:
        raise NotImplementedError

    def _stream(self, rpc: Rpc[Req, Resp], request: Req) -> Stream[Resp]:
        raise NotImplementedError

    def _pages(self, rpc: Rpc[Req, Resp], request: Req) -> Iterator[Resp]:
        """Every page, lazily: the same request with each ``next_page_token``."""
        page_request = rpc.request()
        page_request.CopyFrom(request)
        while True:
            page = self._unary(rpc, page_request)
            yield page
            token: str = page.next_page_token  # type: ignore[attr-defined]
            if not token:
                return
            page_request.page_token = token  # type: ignore[attr-defined]

    def _items(self, rpc: Rpc[Req, Resp], request: Req, field: str) -> Iterator[Any]:
        """Every page's ``field``, item by item."""
        for page in self._pages(rpc, request):
            yield from getattr(page, field)


class AsyncCalls:
    """The asyncio client's call primitives, which the generated methods use."""

    async def _unary(self, rpc: Rpc[Req, Resp], request: Req) -> Resp:
        raise NotImplementedError

    def _stream(self, rpc: Rpc[Req, Resp], request: Req) -> AsyncStream[Resp]:
        raise NotImplementedError

    async def _pages(self, rpc: Rpc[Req, Resp], request: Req) -> AsyncIterator[Resp]:
        """Every page, lazily: the same request with each ``next_page_token``."""
        page_request = rpc.request()
        page_request.CopyFrom(request)
        while True:
            page = await self._unary(rpc, page_request)
            yield page
            token: str = page.next_page_token  # type: ignore[attr-defined]
            if not token:
                return
            page_request.page_token = token  # type: ignore[attr-defined]

    async def _items(self, rpc: Rpc[Req, Resp], request: Req, field: str) -> AsyncIterator[Any]:
        """Every page's ``field``, item by item."""
        async for page in self._pages(rpc, request):
            for item in getattr(page, field):
                yield item


__all__ = ["AsyncCalls", "Iterable", "SyncCalls", "build"]
