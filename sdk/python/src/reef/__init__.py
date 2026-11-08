"""reef-vcs: the Python client of the Reef daemon (R2.06, ADR 0054).

Reef gives each agent its own workspace: a copy-on-write copy of a git or jj repository, leased
to one process and snapshotted after every write. This package talks to a running daemon (it
never starts one), over its socket or a remote listener::

    import reef

    with reef.connect() as client:
        repo = client.register_repository(path=".").repository
        with client.workspace(repo.id, purpose="fix the parser") as ws:
            (ws.path / "parser.py").write_text("...")

Every RPC of ``reef.api.v1`` is a method on :class:`Client` (and, awaited, on
``reef.aio.AsyncClient``), generated from the schema; messages and enums are importable from
``reef.api.v1``. Errors are :class:`ReefError` subclasses by kind, with the daemon's ``hint`` and
``next``.
"""

from reef import _env  # noqa: F401  # before grpc is imported
from reef._build import API_REVISION, COMMIT, VERSION
from reef.client import Client, connect
from reef.errors import (
    AlreadyExistsError,
    ConfigError,
    ConflictError,
    DaemonNotRunningError,
    ErrorKind,
    IncompatibleError,
    InternalError,
    InvalidArgumentError,
    IoError,
    NotFoundError,
    PermissionDeniedError,
    ReefError,
    RemoteConnectError,
    RemoteMisdirectedError,
    RemoteTlsError,
    UnavailableError,
    UnsupportedError,
)
from reef.provenance import Provenance, instruction_sha256
from reef.workspace import WorkspaceHandle

__version__ = VERSION

__all__ = [
    "API_REVISION",
    "COMMIT",
    "AlreadyExistsError",
    "Client",
    "ConfigError",
    "ConflictError",
    "DaemonNotRunningError",
    "ErrorKind",
    "IncompatibleError",
    "InternalError",
    "InvalidArgumentError",
    "IoError",
    "NotFoundError",
    "PermissionDeniedError",
    "Provenance",
    "ReefError",
    "RemoteConnectError",
    "RemoteMisdirectedError",
    "RemoteTlsError",
    "UnavailableError",
    "UnsupportedError",
    "WorkspaceHandle",
    "__version__",
    "connect",
    "instruction_sha256",
]
