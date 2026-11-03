"""Who is calling: the agent name, and a workspace's default owner, as ``reef-client`` has them."""

from __future__ import annotations

import os
import socket

from reef.errors import InvalidArgumentError

AGENT_ENV = "REEF_AGENT"
AGENT_METADATA = "reef-agent"
AGENT_MAX_BYTES = 256


def check_agent(agent: str) -> str | None:
    """``agent`` trimmed, or None when blank.

    Raises:
        InvalidArgumentError: It is not visible ASCII (spaces allowed) of at most 256 bytes; the
            daemon would refuse it.
    """
    text = agent.strip()
    if not text:
        return None
    visible = all(0x20 <= ord(c) <= 0x7E for c in text)
    if len(text.encode()) > AGENT_MAX_BYTES or not visible:
        raise InvalidArgumentError(
            f"an agent must be visible ASCII of at most {AGENT_MAX_BYTES} bytes: {agent!r}",
            reason="INVALID_AGENT",
        )
    return text


def agent_from(given: str | None) -> str | None:
    """The agent a client sends: ``given``, else ``REEF_AGENT``, checked."""
    if given is not None:
        return check_agent(given)
    return check_agent(os.environ.get(AGENT_ENV, ""))


def presence_agent(given: str | None, owner: str | None, fallback: str) -> str:
    """The agent a presence entry names (R2.11's rule 7): the client's agent, else the
    workspace's owner, else ``fallback``; a name the daemon would refuse falls through.
    """
    for name in (given, owner):
        if name is None:
            continue
        try:
            checked = check_agent(name)
        except InvalidArgumentError:
            continue
        if checked:
            return checked
    return fallback


def os_username() -> str:
    """The user the process runs as: the password database entry, else ``$USER``, else the uid."""
    uid = os.getuid()
    try:
        import pwd

        return pwd.getpwuid(uid).pw_name
    except (ImportError, KeyError):
        return os.environ.get("USER") or str(uid)


def machine_hostname() -> str:
    """The machine's hostname, or ``localhost`` if it has none."""
    return socket.gethostname() or "localhost"


def default_owner() -> str:
    """``<user>@<host>``, as ``reef ws create`` sends when not given an owner."""
    return f"{os_username()}@{machine_hostname()}"


def process_owner() -> str:
    """``<user>@<host>:<pid>``, as ``reef ws run`` owns its workspace."""
    return f"{default_owner()}:{os.getpid()}"
