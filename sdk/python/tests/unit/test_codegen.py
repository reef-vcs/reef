"""The generated surface: one method per RPC on both clients, pages, streams, docstrings."""

from __future__ import annotations

import inspect
import re
import sys
import tomllib
from pathlib import Path

import pytest
from google.protobuf import descriptor_pb2

import reef
import reef.aio
from reef import _rpcs
from reef._stream import Stream
from reef.aio._stream import AsyncStream

SDK = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SDK / "codegen"))
import clientgen  # noqa: E402

GENERATED = Path(reef.__file__).parent


def test_codegen() -> None:
    assert len(_rpcs.ALL) >= 43
    for rpc in _rpcs.ALL:
        name = clientgen.snake(rpc.name)
        sync = getattr(reef.Client, name)
        aio = getattr(reef.aio.AsyncClient, name)
        assert sync.__doc__ and sync.__doc__.strip(), name
        assert aio.__doc__ == sync.__doc__, name
        sync_params = inspect.signature(sync).parameters
        aio_params = inspect.signature(aio).parameters
        assert list(sync_params) == list(aio_params), name
        fields = [field.name for field in rpc.request.DESCRIPTOR.fields]
        assert list(sync_params) == ["self", "_request", *fields], name
        assert all(
            param.kind == inspect.Parameter.KEYWORD_ONLY for param in list(sync_params.values())[2:]
        )
        if rpc.access == "ADMIN":
            assert "ADMIN: local socket only" in sync.__doc__, name
        returns = inspect.signature(sync).return_annotation
        if rpc.streaming:
            assert returns.startswith("Stream["), name
            assert inspect.signature(aio).return_annotation.startswith("AsyncStream["), name
        else:
            assert inspect.iscoroutinefunction(aio), name
        assert hasattr(reef.Client, f"{name}_pages") == rpc.paged, name
        has_all = hasattr(reef.Client, f"{name}_all")
        assert has_all == (rpc.items is not None), name
        if rpc.paged:
            assert hasattr(reef.aio.AsyncClient, f"{name}_pages"), name
    assert not hasattr(reef.Client, "query_changes_all"), "items depend on the request"
    assert _rpcs.GET_DIFF.items == "files"
    assert not hasattr(reef.Client, "read_file_all"), "not paged"
    for name in ("subscribe_operations", "subscribe_presence", "backup", "build_store"):
        assert getattr(_rpcs, name.upper()).streaming


def test_streams_are_typed() -> None:
    assert Stream.__name__ == "Stream" and AsyncStream.__name__ == "AsyncStream"


def paged_descriptor(repeated: int) -> descriptor_pb2.FileDescriptorSet:
    """A file with one paged RPC whose response has ``repeated`` repeated fields."""
    file = descriptor_pb2.FileDescriptorProto(name="reef/api/v1/x.proto", package="reef.api.v1")
    request = file.message_type.add(name="ListThingsRequest")
    request.field.add(name="page_token", number=1, type=9, label=1)
    response = file.message_type.add(name="ListThingsResponse")
    response.field.add(name="next_page_token", number=1, type=9, label=1)
    for index in range(repeated):
        response.field.add(name=f"things{index}", number=2 + index, type=9, label=3)
    service = file.service.add(name="ThingService")
    service.method.add(
        name="ListThings",
        input_type=".reef.api.v1.ListThingsRequest",
        output_type=".reef.api.v1.ListThingsResponse",
    )
    return descriptor_pb2.FileDescriptorSet(file=[file])


def test_a_paged_rpc_with_two_lists_needs_page_items() -> None:
    rpcs, _, _ = clientgen.generate(paged_descriptor(1))
    assert 'items="things0"' in rpcs
    with pytest.raises(clientgen.GenerateError, match="PAGE_ITEMS"):
        clientgen.generate(paged_descriptor(2))


def test_parameters_avoid_keywords() -> None:
    assert clientgen.parameter("from") == "from_"
    assert clientgen.parameter("_request") == "_request_"
    assert clientgen.parameter("repo") == "repo"
    assert clientgen.snake("UploadScipDocuments") == "upload_scip_documents"


def test_floors_match_the_generator() -> None:
    project = tomllib.loads((SDK / "pyproject.toml").read_text())
    requires = project["build-system"]["requires"]
    tools = next(r for r in requires if r.startswith("grpcio-tools=="))
    tools_version = tools.split("==")[1]
    deps = {
        re.split(r"[<>=]", d)[0]: re.search(r">=([\d.]+)", d).group(1)  # type: ignore[union-attr]
        for d in project["project"]["dependencies"]
    }
    grpc_text = (GENERATED / "api" / "v1" / "workspaces_pb2_grpc.py").read_text()
    generated = re.search(r"GRPC_GENERATED_VERSION = '([\d.]+)'", grpc_text)
    assert generated is not None
    assert deps["grpcio"] == generated.group(1) == tools_version
    assert deps["grpcio-status"] == tools_version
    pb_text = (GENERATED / "api" / "v1" / "workspaces_pb2.py").read_text()
    runtime = re.search(r"_runtime_version\.Domain\.PUBLIC,\s*(\d+),\s*(\d+),\s*(\d+)", pb_text)
    assert runtime is not None
    assert deps["protobuf"] == ".".join(runtime.groups())


def test_build_constants() -> None:
    from reef import _build

    assert reef.__version__ == _build.VERSION
    assert isinstance(reef.API_REVISION, int) and reef.API_REVISION >= 14
    cargo = tomllib.loads((SDK.parent.parent / "Cargo.toml").read_text())
    assert cargo["workspace"]["package"]["version"] == _build.CARGO_VERSION
