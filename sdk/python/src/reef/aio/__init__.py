"""The asyncio client: :mod:`reef`'s surface, awaited.

::

    import reef.aio

    async with await reef.aio.connect() as client:
        status = await client.get_status(workspace=ws_id)
        async with client.subscribe_operations(repos=[repo_id]) as stream:
            async for frame in stream:
                ...
"""

from reef.aio.client import AsyncClient, connect
from reef.aio.workspace import AsyncWorkspaceHandle

__all__ = ["AsyncClient", "AsyncWorkspaceHandle", "connect"]
