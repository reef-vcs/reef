"""Requests from keyword fields: the same message as a request object, with conversions."""

from __future__ import annotations

import datetime as dt

import pytest

from reef import _rpcs
from reef._calls import build
from reef.api.v1 import (
    Backend,
    CreateWorkspaceRequest,
    EditFilesRequest,
    FileEdit,
    ListWorkspacesRequest,
    WorkspaceState,
)


def test_kwargs_and_requests() -> None:
    built = build(
        _rpcs.CREATE_WORKSPACE,
        None,
        {"repo": "r", "new_on": "trunk()", "backend": "BACKEND_APFS", "owner": None},
    )
    assert built == CreateWorkspaceRequest(repo="r", new_on="trunk()", backend=Backend.BACKEND_APFS)
    assert built.WhichOneof("start") == "new_on"
    assert build(_rpcs.CREATE_WORKSPACE, None, {"change": "abc"}).WhichOneof("start") == "change"

    # A timedelta for a Duration.
    built = build(_rpcs.CREATE_WORKSPACE, None, {"lease_length": dt.timedelta(minutes=5)})
    assert built.lease_length.seconds == 300

    # Repeated enums, by name or number.
    built = build(
        _rpcs.LIST_WORKSPACES,
        None,
        {"states": ["WORKSPACE_STATE_ACTIVE", WorkspaceState.WORKSPACE_STATE_RELEASED]},
    )
    assert list(built.states) == [1, 3]

    # Repeated messages.
    edit = FileEdit(path="a.txt")
    built = build(_rpcs.EDIT_FILES, None, {"workspace": "w", "edits": [edit]})
    assert built == EditFilesRequest(workspace="w", edits=[edit])

    request = ListWorkspacesRequest(repo="r")
    assert build(_rpcs.LIST_WORKSPACES, request, {"repo": None}) is request
    with pytest.raises(TypeError, match="not both"):
        build(_rpcs.LIST_WORKSPACES, request, {"repo": "x"})
    with pytest.raises(TypeError, match="takes a ListWorkspacesRequest"):
        build(_rpcs.LIST_WORKSPACES, CreateWorkspaceRequest(), {})
    with pytest.raises(ValueError, match="BACKEND_APFS"):
        build(_rpcs.CREATE_WORKSPACE, None, {"backend": "APFS"})
    with pytest.raises(TypeError, match="list"):
        build(_rpcs.LIST_WORKSPACES, None, {"states": "WORKSPACE_STATE_ACTIVE"})


def test_unknown_keyword_names_the_method() -> None:
    import reef

    with pytest.raises(TypeError, match="create_workspace"):
        reef.Client.create_workspace(object(), nope=1)  # type: ignore[arg-type, call-arg]


def test_timestamp_from_datetime() -> None:
    from reef._calls import _convert

    field = ListWorkspacesRequest.DESCRIPTOR.fields_by_name["repo"]
    assert _convert(field, "x") == "x"
    stamp_field = next(
        f
        for message in (CreateWorkspaceRequest,)
        for f in message.DESCRIPTOR.fields
        if f.message_type is not None and f.message_type.full_name == "google.protobuf.Duration"
    )
    duration = _convert(stamp_field, dt.timedelta(seconds=1.5))
    assert (duration.seconds, duration.nanos) == (1, 500_000_000)
