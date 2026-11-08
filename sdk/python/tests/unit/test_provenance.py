"""R3.44: the SDK's provenance declarations, hashed here and never holding the text."""

from __future__ import annotations

import dataclasses
import hashlib

import pytest

import reef
from reef.api.v1 import SessionActor
from reef.provenance import CreatePlan, Provenance, declare_fields


def test_instruction_is_hashed_and_dropped() -> None:
    text = "fix the parser\n"
    declared = Provenance(model="m/1", instruction=text)
    digest = hashlib.sha256(text.encode()).hexdigest()
    assert declared.instruction_sha256 == digest
    assert reef.instruction_sha256(text) == digest
    assert reef.instruction_sha256(text.encode()) == digest
    # The text is nowhere on the object.
    assert text not in repr(declared)
    assert all(value != text for value in dataclasses.asdict(declared).values())
    assert getattr(declared, "instruction", None) is None
    assert reef.instruction_sha256("café") == hashlib.sha256("café".encode()).hexdigest()


def test_bad_arguments() -> None:
    with pytest.raises(TypeError):
        Provenance(instruction="x", instruction_sha256="0" * 64)
    with pytest.raises(ValueError):
        Provenance(actor="robot")
    with pytest.raises(TypeError):
        Provenance.of(["model"])  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        Provenance(identity="alice").message(creating=False)


def test_mapping_and_message() -> None:
    declared = Provenance.of({"model": " m ", "actor": "human", "identity": "alice", "task": ""})
    assert declared is not None
    message = declared.message(creating=True)
    assert message.model == "m"
    assert message.actor == SessionActor.SESSION_ACTOR_HUMAN
    assert message.identity == "alice"
    assert not message.HasField("task"), "a new workspace has nothing to clear"
    cleared = Provenance(task="", instruction_sha256="").message(creating=False)
    assert cleared.HasField("task") and cleared.task == ""
    assert cleared.HasField("instruction_sha256")
    assert not cleared.HasField("model")


def test_declare_fields() -> None:
    message = declare_fields(
        model=None,
        harness="h/1",
        task=None,
        instruction=b"bytes",
        instruction_sha256=None,
        client_session="s-1",
    )
    assert message.harness == "h/1"
    assert message.client_session == "s-1"
    assert message.instruction_sha256 == hashlib.sha256(b"bytes").hexdigest()
    assert not message.HasField("model")


def test_environment_defaults(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("REEF_MODEL", raising=False)
    monkeypatch.delenv("REEF_TASK", raising=False)
    assert CreatePlan.of(None) == CreatePlan(None, False)
    monkeypatch.setenv("REEF_MODEL", "env-model")
    monkeypatch.setenv("REEF_TASK", " T-1 ")
    plan = CreatePlan.of(None)
    assert plan.declaration == Provenance(model="env-model", task="T-1")
    assert not plan.explicit
    plan = CreatePlan.of(Provenance(model="mine"))
    assert plan.declaration is not None
    assert plan.declaration.model == "mine"
    assert plan.declaration.task == "T-1"
    assert plan.explicit
    # Nothing else comes from the environment.
    monkeypatch.setenv("CLAUDECODE", "1")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "s")
    monkeypatch.setenv("AI_AGENT", "claude-code_2-1-316_agent")
    plan = CreatePlan.of(None)
    assert plan.declaration is not None
    assert plan.declaration.harness is None
    assert plan.declaration.client_session is None


def test_bad_environment_values_are_ignored(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("REEF_MODEL", "two words")
    monkeypatch.setenv("REEF_TASK", "tab\there")
    assert CreatePlan.of(None) == CreatePlan(None, False)
    plan = CreatePlan.of(Provenance(task="T-1"))
    assert plan.declaration is not None
    assert plan.declaration.model is None


def test_empty_fields_and_the_empty_instruction(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("REEF_MODEL", raising=False)
    monkeypatch.delenv("REEF_TASK", raising=False)
    # An empty field declares nothing on a new workspace, so it is not explicit.
    assert CreatePlan.of(Provenance(model="")) == CreatePlan(None, False)
    cleared = Provenance(instruction="").message(creating=False)
    assert cleared.HasField("instruction_sha256") and cleared.instruction_sha256 == ""
