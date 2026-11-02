"""Where the daemon listens, settled exactly as ``reef-client``'s ``paths.rs`` settles it.

Precedence, lowest first: the platform default, ``config.toml``'s ``[daemon] socket``, the
environment (``REEF_STATE_DIR``, ``REEF_SOCKET``, ``REEF_CONFIG``), then an argument. Empty
variables are unset; relative paths are made absolute against the current directory. The
``$XDG_RUNTIME_DIR`` socket is the default on Linux only for the default state directory. A socket
path over 103 bytes is an error. ``sdk/fixtures/socket-paths.json`` holds the cases this and the
Rust implementation both pass.
"""

from __future__ import annotations

import os
import sys
import tomllib
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path

from reef.errors import ConfigError, ErrorKind

STATE_DIR_ENV = "REEF_STATE_DIR"
SOCKET_ENV = "REEF_SOCKET"
CONFIG_ENV = "REEF_CONFIG"
SOCKET_PATH_MAX = 103

Env = Callable[[str], "str | None"]


@dataclass(frozen=True)
class PlatformDefaults:
    """The platform's default state directory and, on Linux, its runtime directory."""

    state_dir: str
    runtime_dir: str | None = None


@dataclass(frozen=True)
class Settled:
    """The paths a client needs."""

    state_dir: str
    config_file: str
    socket: str


def _absolute_xdg(value: str | None) -> str | None:
    # etcetera, which the daemon uses, ignores a relative or empty XDG variable.
    if value and os.path.isabs(value):
        return value
    return None


def platform_defaults(
    env: Env, home: str | None = None, platform: str | None = None
) -> PlatformDefaults:
    """This platform's defaults, from the home directory and the XDG variables."""
    platform = platform or sys.platform
    if home is None:
        home = env("HOME") or None
        if home is None:
            try:
                home = str(Path.home())
            except RuntimeError:
                home = None
    if not home:
        raise ConfigError(
            f"cannot find the home directory for the default state directory; set {STATE_DIR_ENV}"
        )
    if platform == "darwin":
        return PlatformDefaults(os.path.join(home, "Library", "Application Support", "reef"))
    state = _absolute_xdg(env("XDG_STATE_HOME")) or os.path.join(home, ".local", "state")
    return PlatformDefaults(
        state_dir=os.path.join(state, "reef"),
        runtime_dir=_absolute_xdg(env("XDG_RUNTIME_DIR")),
    )


def _absolute(path: str, cwd: str | None) -> str:
    """``path`` made absolute as Rust's ``std::path::absolute`` makes it on Unix.

    Relative paths are joined to the current directory; repeated separators and ``.`` components
    go, while ``..`` and a trailing separator stay. Nothing on disk is read.
    """
    if not path:
        raise ConfigError("resolve : the path is empty", kind=ErrorKind.IO)
    if not os.path.isabs(path):
        base = cwd if cwd is not None else os.getcwd()
        path = base.rstrip("/") + "/" + path
    lead = "//" if path.startswith("//") and not path.startswith("///") else "/"
    parts = [part for part in path.split("/") if part not in ("", ".")]
    trailing = "/" if path.endswith("/") and parts else ""
    return lead + "/".join(parts) + trailing


def settle_state_dir(
    *,
    state_dir: str | os.PathLike[str] | None = None,
    env: Env | None = None,
    cwd: str | None = None,
) -> str:
    """The state directory alone: the argument, else ``REEF_STATE_DIR``, else the default."""
    get: Env = env or os.environ.get
    if state_dir is not None:
        return _absolute(os.fspath(state_dir), cwd)
    from_env = get(STATE_DIR_ENV)
    if from_env:
        return _absolute(from_env, cwd)
    return platform_defaults(get).state_dir


def settle(
    *,
    socket: str | os.PathLike[str] | None = None,
    state_dir: str | os.PathLike[str] | None = None,
    config: str | os.PathLike[str] | None = None,
    env: Env | Mapping[str, str] | None = None,
    defaults: Callable[[], PlatformDefaults] | None = None,
    cwd: str | None = None,
) -> Settled:
    """The state directory, config file and socket, as the CLI and the daemon settle them.

    Raises:
        ConfigError: The paths cannot be settled, or the config file cannot be read.
    """
    if env is None:
        get: Env = os.environ.get
    elif isinstance(env, Mapping):
        get = env.get
    else:
        get = env

    def given(value: str | os.PathLike[str] | None, var: str) -> tuple[str, str] | None:
        if value is not None:
            return _absolute(os.fspath(value), cwd), "argument"
        from_env = get(var)
        if from_env:
            return _absolute(from_env, cwd), "env"
        return None

    state = given(state_dir, STATE_DIR_ENV)
    sock = given(socket, SOCKET_ENV)
    conf = given(config, CONFIG_ENV)

    runtime_dir: str | None = None
    if state is not None:
        state_path = state[0]
    else:
        platform = (defaults or (lambda: platform_defaults(get)))()
        state_path, runtime_dir = platform.state_dir, platform.runtime_dir
    config_file, config_source = conf if conf else (os.path.join(state_path, "config.toml"), "")

    file_socket = _read_socket_key(config_file, config_source)
    if sock is not None:
        socket_path = sock[0]
    elif file_socket is not None:
        socket_path = os.path.join(state_path, file_socket)
    elif runtime_dir is not None:
        socket_path = os.path.join(runtime_dir, "reef", "daemon.sock")
    else:
        socket_path = os.path.join(state_path, "daemon.sock")
    length = len(os.fsencode(socket_path))
    if length > SOCKET_PATH_MAX:
        raise ConfigError(
            f"socket path {socket_path} is {length} bytes; the limit is {SOCKET_PATH_MAX}. "
            f"Set {SOCKET_ENV} or use a shorter state directory"
        )
    return Settled(state_dir=state_path, config_file=config_file, socket=socket_path)


def _read_socket_key(path: str, source: str) -> str | None:
    """``[daemon] socket`` from the config file; the rest of the file is the daemon's."""
    try:
        with open(path, "rb") as file:
            text = file.read()
    except FileNotFoundError:
        if source:
            named = "the config argument" if source == "argument" else CONFIG_ENV
            raise ConfigError(
                f"the configuration file {path} does not exist (named by {named})",
                kind=ErrorKind.NOT_FOUND,
            ) from None
        return None
    except PermissionError as error:
        raise ConfigError(
            f"read the configuration file {path}: {error.strerror}",
            kind=ErrorKind.PERMISSION_DENIED,
        ) from error
    except OSError as error:
        raise ConfigError(
            f"read the configuration file {path}: {error.strerror}", kind=ErrorKind.IO
        ) from error
    try:
        data = tomllib.loads(text.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise ConfigError(f"parse the configuration file {path}: {error}") from error
    # The three keys a client reads, typed as the Rust client types them; the rest is ignored.
    daemon = _table(data, "daemon", path)
    repo = _table(data, "repo", path)
    for table, key, kind in ((daemon, "pid_file", str), (repo, "auto_register", bool)):
        if key in table and not isinstance(table[key], kind):
            raise ConfigError(f"parse the configuration file {path}: {key} has the wrong type")
    value = daemon.get("socket")
    if value is None:
        return None
    if not isinstance(value, str):
        raise ConfigError(f"parse the configuration file {path}: daemon.socket is not a string")
    return value


def _table(data: dict[str, object], key: str, path: str) -> dict[str, object]:
    value = data.get(key, {})
    if not isinstance(value, dict):
        raise ConfigError(f"parse the configuration file {path}: [{key}] is not a table")
    return value
