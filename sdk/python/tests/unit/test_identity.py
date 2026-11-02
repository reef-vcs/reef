"""Agent names and owners, as reef-client checks and makes them."""

from __future__ import annotations

import os

import pytest

import reef
from reef import _identity
from reef.workspace import may_attach


def test_agent_and_owner(monkeypatch: pytest.MonkeyPatch) -> None:
    check = _identity.check_agent
    assert check("  bot-1 ") == "bot-1"
    assert check("") is None
    assert check("   ") is None
    assert check("planner 3") == "planner 3"
    assert check("x" * 256) == "x" * 256
    for bad in ("x" * 257, "a\tb", "a\nb", "équipe"):
        with pytest.raises(reef.InvalidArgumentError) as raised:
            check(bad)
        assert raised.value.reason == "INVALID_AGENT"

    monkeypatch.setenv("REEF_AGENT", "from-env")
    assert _identity.agent_from(None) == "from-env"
    assert _identity.agent_from("given") == "given"
    monkeypatch.delenv("REEF_AGENT")
    assert _identity.agent_from(None) is None

    owner = _identity.process_owner()
    user_host, pid = owner.rsplit(":", 1)
    assert pid == str(os.getpid())
    assert user_host == _identity.default_owner()
    assert "@" in user_host


def test_attach_rule() -> None:
    assert may_attach("u@h", None, "u@h")
    assert may_attach("u@h:12", None, "u@h")
    assert not may_attach("u@hx", None, "u@h")
    assert not may_attach("v@h", None, "u@h")
    assert may_attach("bot", "bot", "u@h")
    assert not may_attach("u@h", "bot", "u@h")
