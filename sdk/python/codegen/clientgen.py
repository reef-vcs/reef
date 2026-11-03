"""Writes the clients' methods from the schema's descriptors (R2.06, ADR 0054).

One method per RPC, named in snake case, on the sync and asyncio clients alike: every request
field is a keyword-only parameter, or a whole request is passed positionally. A paged RPC (a
``page_token`` in, a ``next_page_token`` out) also gets ``<method>_pages``, and ``<method>_all``
where its items are known: the response's one repeated field, or ``PAGE_ITEMS``' entry
(``sdk/fixtures/page-items.json``). A
server-streaming RPC returns a stream. Docstrings are the schema's comments.

The output is what ``ruff format`` leaves unchanged.
"""

from __future__ import annotations

import json
import keyword
import re
from dataclasses import dataclass, field
from pathlib import Path

from google.protobuf import descriptor_pb2

PACKAGE = "reef.api.v1"

# A paged RPC whose response has more than one repeated field names its items in
# sdk/fixtures/page-items.json, which the TypeScript generator reads too; `None` means its items
# depend on the request, so it gets `_pages` only. ADR 0029's checklist, item 12.
PAGE_ITEMS_FILE = Path(__file__).resolve().parents[2] / "fixtures" / "page-items.json"
PAGE_ITEMS: dict[str, str | None] = json.loads(PAGE_ITEMS_FILE.read_text())["items"]

# `(reef.api.v1.access)` on MethodOptions, options.proto.
ACCESS_FIELD = 50701
ACCESS_NAMES = {1: "READ", 2: "WRITE", 3: "ADMIN"}

_T = descriptor_pb2.FieldDescriptorProto
SCALARS = {
    _T.TYPE_DOUBLE: "float",
    _T.TYPE_FLOAT: "float",
    _T.TYPE_INT64: "int",
    _T.TYPE_UINT64: "int",
    _T.TYPE_INT32: "int",
    _T.TYPE_FIXED64: "int",
    _T.TYPE_FIXED32: "int",
    _T.TYPE_BOOL: "bool",
    _T.TYPE_STRING: "str",
    _T.TYPE_BYTES: "bytes",
    _T.TYPE_UINT32: "int",
    _T.TYPE_SFIXED32: "int",
    _T.TYPE_SFIXED64: "int",
    _T.TYPE_SINT32: "int",
    _T.TYPE_SINT64: "int",
}
WELL_KNOWN = {
    ".google.protobuf.Timestamp": "_dt.datetime | _Timestamp",
    ".google.protobuf.Duration": "_dt.timedelta | _Duration",
}


class GenerateError(Exception):
    """The schema has a shape the generator cannot settle by itself."""


def snake(name: str) -> str:
    """``CreateWorkspace`` as ``create_workspace``."""
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def constant(name: str) -> str:
    """``CreateWorkspace`` as ``CREATE_WORKSPACE``."""
    return snake(name).upper()


def parameter(name: str) -> str:
    """A field's parameter name: the field's, with ``_`` after a Python keyword."""
    if keyword.iskeyword(name) or name in {"self", "_request"}:
        return name + "_"
    return name


def _varint(data: bytes, at: int) -> tuple[int, int]:
    value = shift = 0
    while True:
        byte = data[at]
        at += 1
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, at
        shift += 7


def access_of(options: descriptor_pb2.MethodOptions) -> str:
    """The method's ``(reef.api.v1.access)``, read from the options' wire form."""
    data = options.SerializeToString()
    at = 0
    while at < len(data):
        key, at = _varint(data, at)
        number, wire = key >> 3, key & 7
        if wire == 0:
            value, at = _varint(data, at)
            if number == ACCESS_FIELD:
                return ACCESS_NAMES.get(value, "UNSPECIFIED")
        elif wire == 2:
            length, at = _varint(data, at)
            at += length
        elif wire == 1:
            at += 8
        elif wire == 5:
            at += 4
        else:
            raise GenerateError(f"unexpected wire type {wire} in method options")
    return "UNSPECIFIED"


@dataclass
class Field:
    """A request field as a parameter."""

    name: str
    param: str
    annotation: str
    comment: str


@dataclass
class Method:
    """One RPC."""

    service: str
    name: str
    request: str
    response: str
    access: str
    streaming: bool
    comment: str
    fields: list[Field] = field(default_factory=list)
    paged: bool = False
    items: str | None = None
    item_type: str | None = None

    @property
    def path(self) -> str:
        """The gRPC method path."""
        return f"/{PACKAGE}.{self.service}/{self.name}"

    @property
    def python(self) -> str:
        """The method's name on the clients."""
        return snake(self.name)

    @property
    def const(self) -> str:
        """The RPC's name in ``reef._rpcs``."""
        return constant(self.name)


class Schema:
    """The descriptors, indexed by full name, with comments."""

    def __init__(self, descriptors: descriptor_pb2.FileDescriptorSet) -> None:
        self.messages: dict[str, descriptor_pb2.DescriptorProto] = {}
        self.enums: set[str] = set()
        self.comments: dict[tuple[str, tuple[int, ...]], str] = {}
        self.message_paths: dict[str, tuple[str, tuple[int, ...]]] = {}
        self.files = list(descriptors.file)
        for file in self.files:
            for location in file.source_code_info.location:
                text = location.leading_comments.strip("\n")
                if text:
                    self.comments[(file.name, tuple(location.path))] = text
            prefix = f".{file.package}" if file.package else ""
            for index, message in enumerate(file.message_type):
                self._index(file.name, prefix, message, (4, index))
            for enum in file.enum_type:
                self.enums.add(f"{prefix}.{enum.name}")

    def _index(
        self,
        file: str,
        prefix: str,
        message: descriptor_pb2.DescriptorProto,
        path: tuple[int, ...],
    ) -> None:
        name = f"{prefix}.{message.name}"
        self.messages[name] = message
        self.message_paths[name] = (file, path)
        for index, nested in enumerate(message.nested_type):
            self._index(file, name, nested, (*path, 3, index))
        for enum in message.enum_type:
            self.enums.add(f"{name}.{enum.name}")

    def _is_map(self, type_name: str) -> bool:
        entry = self.messages.get(type_name)
        return entry is not None and entry.options.map_entry

    def comment(self, file: str, path: tuple[int, ...]) -> str:
        """The leading comment at ``path`` in ``file``, dedented by one space."""
        text = self.comments.get((file, path), "")
        return "\n".join(line[1:] if line.startswith(" ") else line for line in text.split("\n"))

    def python_type(self, full_name: str) -> str:
        """A message or enum's class in the generated package."""
        if not full_name.startswith(f".{PACKAGE}."):
            raise GenerateError(f"{full_name} is outside {PACKAGE}")
        return "_v1." + full_name[len(PACKAGE) + 2 :]

    def annotation(self, f: descriptor_pb2.FieldDescriptorProto) -> str:
        """The parameter's type, without ``| None``."""
        if f.type == _T.TYPE_MESSAGE:
            entry = self.messages.get(f.type_name)
            if entry is not None and entry.options.map_entry:
                key = self.element(entry.field[0])
                value = self.element(entry.field[1])
                return f"_Mapping[{key}, {value}]"
        element = self.element(f)
        if f.label == _T.LABEL_REPEATED:
            return f"_Iterable[{element}]"
        return element

    def element(self, f: descriptor_pb2.FieldDescriptorProto) -> str:
        """One value's type."""
        if f.type in SCALARS:
            return SCALARS[f.type]
        if f.type == _T.TYPE_ENUM:
            return f"{self.python_type(f.type_name)}.ValueType | str"
        if f.type == _T.TYPE_MESSAGE:
            if f.type_name in WELL_KNOWN:
                return WELL_KNOWN[f.type_name]
            return self.python_type(f.type_name)
        raise GenerateError(f"field {f.name} has an unsupported type {f.type}")

    def methods(self) -> list[Method]:
        """Every RPC in the package, in file order."""
        methods: list[Method] = []
        for file in self.files:
            if file.package != PACKAGE:
                continue
            for s_index, service in enumerate(file.service):
                for m_index, rpc in enumerate(service.method):
                    if rpc.client_streaming:
                        raise GenerateError(f"{rpc.name} streams from the client")
                    method = Method(
                        service=service.name,
                        name=rpc.name,
                        request=self.python_type(rpc.input_type),
                        response=self.python_type(rpc.output_type),
                        access=access_of(rpc.options),
                        streaming=rpc.server_streaming,
                        comment=self.comment(file.name, (6, s_index, 2, m_index)),
                    )
                    request = self.messages[rpc.input_type]
                    r_file, r_path = self.message_paths[rpc.input_type]
                    for f_index, f in enumerate(request.field):
                        method.fields.append(
                            Field(
                                name=f.name,
                                param=parameter(f.name),
                                annotation=self.annotation(f),
                                comment=self.comment(r_file, (*r_path, 2, f_index)),
                            )
                        )
                    self._pages(method, request, self.messages[rpc.output_type])
                    methods.append(method)
        return methods

    def _pages(
        self,
        method: Method,
        request: descriptor_pb2.DescriptorProto,
        response: descriptor_pb2.DescriptorProto,
    ) -> None:
        names = {f.name for f in request.field}
        if "page_token" not in names or not any(
            f.name == "next_page_token" for f in response.field
        ):
            return
        if method.streaming:
            raise GenerateError(f"{method.name} is paged and streams")
        method.paged = True
        repeated = [
            f
            for f in response.field
            if f.label == _T.LABEL_REPEATED
            and not (f.type == _T.TYPE_MESSAGE and self._is_map(f.type_name))
        ]
        if method.name in PAGE_ITEMS:
            items = PAGE_ITEMS[method.name]
        elif len(repeated) == 1:
            items = repeated[0].name
        else:
            raise GenerateError(
                f"{method.name} is paged and its response has {len(repeated)} repeated fields; "
                "name its items in sdk/fixtures/page-items.json"
            )
        if items is None:
            return
        chosen = [f for f in repeated if f.name == items]
        if not chosen:
            raise GenerateError(f"PAGE_ITEMS names {method.name}.{items}, which is not repeated")
        method.items = items
        method.item_type = self.element(chosen[0])


HEADER = '''"""{doc} Generated from the schema by codegen/clientgen.py; do not edit."""

from __future__ import annotations

{imports}
'''


def _docstring(method: Method, indent: str, extra: str = "") -> str:
    parts: list[str] = []
    summary = method.comment.strip() or f"``{PACKAGE}.{method.service}.{method.name}``."
    parts.append(summary)
    notes = [f"``{PACKAGE}.{method.service}.{method.name}``, access {method.access}."]
    if method.access == "ADMIN":
        notes.append("ADMIN: local socket only; a remote daemon refuses it.")
    if extra:
        notes.append(extra)
    parts.append(" ".join(notes))
    documented = [f for f in method.fields if f.comment.strip()]
    if documented:
        lines = ["Args:"]
        for f in documented:
            comment = f.comment.strip().split("\n")
            lines.append(f"    {f.param}: {comment[0]}".rstrip())
            lines.extend(f"        {line}".rstrip() if line.strip() else "" for line in comment[1:])
        parts.append("\n".join(lines))
    text = "\n\n".join(parts).replace("\\", "\\\\").replace('"""', '\\"\\"\\"')
    if text.endswith('"'):
        text += " "
    body = "\n".join(f"{indent}{line}" if line.strip() else "" for line in text.split("\n"))
    return f'{indent}"""{body.lstrip()}\n{indent}"""\n'


def _signature(method: Method, name: str, returns: str, prefix: str = "def") -> str:
    lines = [
        f"    {prefix} {name}(",
        "        self,",
        f"        _request: {method.request} | None = None,",
        "        /,",
    ]
    if method.fields:
        lines.append("        *,")
        lines.extend(f"        {f.param}: {f.annotation} | None = None," for f in method.fields)
    lines.append(f"    ) -> {returns}:")
    return "\n".join(lines) + "\n"


def _request(method: Method) -> str:
    """The statement building the request, indented for a method body."""
    if not method.fields:
        return f"        request = _build(_rpcs.{method.const}, _request, {{}})\n"
    entries = "".join(f'                "{f.name}": {f.param},\n' for f in method.fields)
    return (
        "        request = _build(\n"
        f"            _rpcs.{method.const},\n"
        "            _request,\n"
        "            {\n"
        f"{entries}"
        "            },\n"
        "        )\n"
    )


def rpcs_module(methods: list[Method]) -> str:
    """``reef/_rpcs.py``: every RPC's path, types, access and paging."""
    header = HEADER.format(
        doc="Every RPC in reef.api.v1.",
        imports=(
            "from typing import Any\n\nfrom reef._rpc import Rpc\nfrom reef.api import v1 as _v1"
        ),
    )
    blocks: list[str] = []
    for m in methods:
        items = f'"{m.items}"' if m.items else "None"
        blocks.append(
            f"{m.const}: Rpc[{m.request}, {m.response}] = Rpc(\n"
            f'    path="{m.path}",\n'
            f'    name="{m.name}",\n'
            f"    request={m.request},\n"
            f"    response={m.response},\n"
            f'    access="{m.access}",\n'
            f"    streaming={m.streaming},\n"
            f"    paged={m.paged},\n"
            f"    items={items},\n"
            ")\n"
        )
    table = (
        "ALL: tuple[Rpc[Any, Any], ...] = (\n"
        + "".join(f"    {m.const},\n" for m in methods)
        + ")\n"
        + "BY_PATH: dict[str, Rpc[Any, Any]] = {rpc.path: rpc for rpc in ALL}\n"
    )
    return header + "\n" + "\n".join(blocks) + "\n" + table


def _imports(methods: list[Method], asynchronous: bool) -> str:
    """The module's imports, only those its annotations use, as ruff and isort order them."""
    text = " ".join(f.annotation for m in methods for f in m.fields)
    abc = ["AsyncIterator" if asynchronous else "Iterator"]
    abc += [name for name in ("Iterable", "Mapping") if f"_{name}[" in text]
    abc.sort()
    # isort's default: one `from ... import X as Y` per line.
    abc_line = "\n".join(f"from collections.abc import {name} as _{name}" for name in abc)
    lines = []
    if "_dt." in text:
        lines.append("import datetime as _dt")
    lines.append(abc_line)
    google = []
    if "_Duration" in text:
        google.append("from google.protobuf.duration_pb2 import Duration as _Duration")
    if "_Timestamp" in text:
        google.append("from google.protobuf.timestamp_pb2 import Timestamp as _Timestamp")
    if google:
        lines += ["", *google]
    if asynchronous:
        base, stream = (
            "from reef._calls import AsyncCalls",
            "from reef.aio._stream import AsyncStream",
        )
    else:
        base, stream = "from reef._calls import SyncCalls", "from reef._stream import Stream"
    lines += [
        "",
        "from reef import _rpcs",
        base,
        "from reef._calls import build as _build",
        stream,
        "from reef.api import v1 as _v1",
    ]
    return "\n".join(lines)


def methods_module(methods: list[Method], *, asynchronous: bool) -> str:
    """``reef/_methods.py`` or ``reef/aio/_methods.py``."""
    if asynchronous:
        base, stream, iterator = "AsyncCalls", "AsyncStream", "_AsyncIterator"
        doc = "The asyncio client's methods, one per RPC."
    else:
        base, stream, iterator = "SyncCalls", "Stream", "_Iterator"
        doc = "The sync client's methods, one per RPC."
    imports = _imports(methods, asynchronous)
    out = [HEADER.format(doc=doc, imports=imports), "\n\n"]
    out.append(f"class Methods({base}):\n")
    out.append(f'    """{doc}"""\n')
    for m in methods:
        out.append("\n")
        request = _request(m)
        if m.streaming:
            out.append(_signature(m, m.python, f"{stream}[{m.response}]"))
            extra = "Returns a stream; leaving its `with` cancels it."
            out.append(_docstring(m, "        ", extra))
            out.append(request)
            out.append(f"        return self._stream(_rpcs.{m.const}, request)\n")
            continue
        prefix = "async def" if asynchronous else "def"
        awaited = "await " if asynchronous else ""
        out.append(_signature(m, m.python, m.response, prefix))
        out.append(_docstring(m, "        "))
        out.append(request)
        out.append(f"        return {awaited}self._unary(_rpcs.{m.const}, request)\n")
        if not m.paged:
            continue
        out.append("\n")
        out.append(_signature(m, f"{m.python}_pages", f"{iterator}[{m.response}]"))
        out.append(_docstring(m, "        ", "Yields every page, following `next_page_token`."))
        out.append(request)
        out.append(f"        return self._pages(_rpcs.{m.const}, request)\n")
        if m.items and m.item_type:
            out.append("\n")
            out.append(_signature(m, f"{m.python}_all", f"{iterator}[{m.item_type}]"))
            extra = f"Yields every page's `{m.items}`, following `next_page_token`."
            out.append(_docstring(m, "        ", extra))
            out.append(request)
            out.append(f'        return self._items(_rpcs.{m.const}, request, "{m.items}")\n')
    return "".join(out)


def generate(descriptors: descriptor_pb2.FileDescriptorSet) -> tuple[str, str, str]:
    """``_rpcs.py``, ``_methods.py`` and ``aio/_methods.py``."""
    methods = Schema(descriptors).methods()
    if not methods:
        raise GenerateError(f"no RPCs in {PACKAGE}")
    return (
        rpcs_module(methods),
        methods_module(methods, asynchronous=False),
        methods_module(methods, asynchronous=True),
    )
