"""Choosing a remote daemon (R2.08, ADR 0049), as ``reef-client``'s ``remote.rs`` and the CLI do.

``REEF_REMOTE`` (or the ``remote`` argument) is a profile's name in ``<state>/remotes.toml``, or an
``https://`` URL, which then needs a token. ``REEF_REMOTE_TOKEN`` and ``REEF_REMOTE_FINGERPRINT``
(or the arguments) override a profile's. ``sdk/fixtures/remote-urls.json`` holds the URL cases
this and the Rust implementation both pass.
"""

from __future__ import annotations

import ipaddress
import os
import tomllib
from collections.abc import Callable
from dataclasses import dataclass, field

from reef.errors import ConfigError, ErrorKind

REMOTE_ENV = "REEF_REMOTE"
TOKEN_ENV = "REEF_REMOTE_TOKEN"
FINGERPRINT_ENV = "REEF_REMOTE_FINGERPRINT"
PROFILES_FILE = "remotes.toml"


@dataclass(frozen=True)
class RemoteUrl:
    """An ``https://`` URL's host and port."""

    host: str
    """A DNS name, or an IP address without brackets."""
    port: int

    @classmethod
    def parse(cls, text: str) -> RemoteUrl | None:
        """``text`` if it is ``https://<host>[:<port>][/]``, else None."""
        if not text.startswith("https://"):
            return None
        rest = text[len("https://") :]
        rest = rest.removesuffix("/")
        if not rest or any(c in rest for c in "/?#@"):
            return None
        if rest.startswith("["):
            host, sep, after = rest[1:].partition("]")
            if not sep or "%" in host:  # Rust's Ipv6Addr takes no zone id
                return None
            try:
                ipaddress.IPv6Address(host)
            except ValueError:
                return None
            if after == "":
                port: int | None = 443
            elif after.startswith(":"):
                port = _port(after[1:])
            else:
                return None
        else:
            host, sep, port_text = rest.rpartition(":")
            if sep:
                port = _port(port_text)
            else:
                host, port = rest, 443
        if port is None:
            return None
        if ":" in host and "[" not in text:
            return None
        if not host or port == 0:
            return None
        return cls(host, port)

    @property
    def authority(self) -> str:
        """``<host>:<port>``, bracketing an IPv6 host."""
        if ":" in self.host:
            return f"[{self.host}]:{self.port}"
        return f"{self.host}:{self.port}"

    @property
    def origin(self) -> str:
        """``https://<authority>``."""
        return f"https://{self.authority}"


def _port(text: str) -> int | None:
    # Rust's u16 parse: ASCII digits only (an optional `+`), at most 65535.
    digits = text.removeprefix("+")
    if not digits or not digits.isascii() or not digits.isdigit():
        return None
    value = int(digits)
    return value if value <= 65535 else None


def is_fingerprint(text: str) -> bool:
    """Whether ``text`` is ``sha256:`` and 64 lowercase hex digits."""
    hexpart = text.removeprefix("sha256:")
    return (
        text.startswith("sha256:")
        and len(hexpart) == 64
        and all(c in "0123456789abcdef" for c in hexpart)
    )


@dataclass(frozen=True)
class RemoteTarget:
    """A remote daemon to connect to."""

    url: RemoteUrl
    token: str = field(repr=False)
    fingerprint: str | None = None
    name: str | None = None
    """The profile's name; None for a URL."""

    @property
    def label(self) -> str:
        """How errors name it: the profile's name, else the URL."""
        return self.name or self.url.origin


def choose(
    remote: str,
    *,
    state_dir: str,
    token: str | None = None,
    fingerprint: str | None = None,
    env: Callable[[str], str | None] = os.environ.get,
) -> RemoteTarget:
    """The target ``remote`` names, with the token and pin from the arguments or environment.

    Raises:
        ConfigError: A URL without a token, an unknown profile, or a malformed one.
    """

    def from_env(name: str) -> str | None:
        value = env(name)
        return value if value and value.strip() else None

    token = (token or "").strip() or None
    if token is None:
        env_token = from_env(TOKEN_ENV)
        token = env_token.strip() if env_token else None
    fingerprint = fingerprint or from_env(FINGERPRINT_ENV)
    if remote.startswith("https://"):
        url = RemoteUrl.parse(remote)
        if url is None:
            raise ConfigError(f"{remote!r} is not https://<host>[:<port>]")
        if token is None:
            raise ConfigError(
                f"{REMOTE_ENV} names a URL, and {TOKEN_ENV} is not set",
                hint=f"Set {TOKEN_ENV} to the token, or save a profile with `reef remote add`.",
            )
        return RemoteTarget(url=url, token=token, fingerprint=fingerprint)
    profiles = load_profiles(state_dir)
    profile = profiles.get(remote)
    if profile is None:
        raise ConfigError(
            f"no remote named {remote}",
            kind=ErrorKind.NOT_FOUND,
            hint="`reef remote list` lists the saved ones; `reef remote add` saves one.",
            next=["reef", "remote", "list"],
        )
    url = RemoteUrl.parse(str(profile.get("url", "")))
    if url is None:
        raise ConfigError(f"the remote {remote} has an invalid URL {profile.get('url')!r}")
    pinned = profile.get("fingerprint")
    return RemoteTarget(
        url=url,
        token=token or str(profile.get("token", "")).strip(),
        fingerprint=fingerprint or (pinned if isinstance(pinned, str) else None),
        name=remote,
    )


def load_profiles(state_dir: str) -> dict[str, dict[str, object]]:
    """``<state>/remotes.toml``'s profiles; none when the file is absent."""
    path = os.path.join(state_dir, PROFILES_FILE)
    try:
        with open(path, "rb") as file:
            data = tomllib.load(file)
    except FileNotFoundError:
        return {}
    except tomllib.TOMLDecodeError as error:
        raise ConfigError(f"{path} is not a valid profiles file: {error}") from error
    except OSError as error:
        raise ConfigError(f"{path}: {error.strerror}", kind=ErrorKind.IO) from error
    profiles: dict[str, dict[str, object]] = {}
    for name, profile in data.items():
        if (
            not isinstance(profile, dict)
            or not isinstance(profile.get("url"), str)
            or not isinstance(profile.get("token"), str)
            or set(profile) - {"url", "token", "fingerprint"}
        ):
            raise ConfigError(f"{path} is not a valid profiles file: the profile {name} is wrong")
        profiles[name] = profile
    return profiles
