"""What a workspace's work is declared to be for: its provenance context (R3.04, R3.44).

Every commit the daemon records for a workspace gets a signed provenance record naming the context
declared for it: the model, the harness, the task, the instruction and the harness's own session.
``client.workspace(..., provenance=...)`` declares it at creation, and ``handle.declare(...)``
replaces fields later. An instruction is given as its text and hashed here with SHA-256 over its
exact UTF-8 bytes: only the digest is kept and sent, never the text. Everything declared is a
claim the daemon records, not something it verifies.
"""

from __future__ import annotations

import hashlib
import os
import unicodedata
from collections.abc import Callable, Mapping
from dataclasses import InitVar, dataclass
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from reef.api.v1 import ProvenanceDeclaration

#: The API revision that added provenance (R3.04).
PROVENANCE_REVISION = 18

#: The environment variables a new workspace's model and task default to, as ``reef ws create``.
MODEL_ENV = "REEF_MODEL"
TASK_ENV = "REEF_TASK"

_ACTORS = ("agent", "executor", "human")

#: The daemon's limits on a declared label (model, harness, client session) and a task.
MAX_LABEL_BYTES = 128
MAX_TASK_BYTES = 512


def is_label(value: str) -> bool:
    """Whether a record can carry ``value`` as a label: 1 to 128 bytes of visible ASCII."""
    return 0 < len(value.encode()) <= MAX_LABEL_BYTES and all("!" <= c <= "~" for c in value)


def is_task(value: str) -> bool:
    """Whether a record can carry ``value`` as a task: 1 to 512 bytes, no control characters."""
    return 0 < len(value.encode()) <= MAX_TASK_BYTES and not any(
        unicodedata.category(c) == "Cc" for c in value
    )


_CONTEXT = ("model", "harness", "task", "instruction_sha256", "client_session")


def instruction_sha256(instruction: str | bytes) -> str:
    """The SHA-256 of an instruction's exact bytes (UTF-8 for text), as lowercase hex.

    Nothing is trimmed: ``printf %s "$text" | shasum -a 256`` gives the same digest.
    """
    data = instruction.encode() if isinstance(instruction, str) else bytes(instruction)
    return hashlib.sha256(data).hexdigest()


@dataclass(frozen=True)
class Provenance:
    """A declaration: each field ``None`` to leave it, ``""`` to clear it (on ``declare``).

    ``instruction`` is the instruction's text (``str`` or ``bytes``); it is hashed when the object
    is made and never kept. ``identity`` and ``actor`` (``"agent"``, ``"executor"`` or
    ``"human"``) bind the workspace's provenance session and are taken only at creation.
    """

    model: str | None = None
    harness: str | None = None
    task: str | None = None
    instruction_sha256: str | None = None
    client_session: str | None = None
    identity: str | None = None
    actor: str | None = None
    instruction: InitVar[str | bytes | None] = None

    def __post_init__(self, instruction: str | bytes | None) -> None:
        if instruction is not None:
            if self.instruction_sha256 is not None:
                raise TypeError("give instruction or instruction_sha256, not both")
            # An empty instruction clears it, as an empty field does.
            digest = instruction_sha256(instruction) if len(instruction) else ""
            object.__setattr__(self, "instruction_sha256", digest)
        if self.actor is not None and self.actor not in _ACTORS:
            raise ValueError(f"actor is one of {', '.join(_ACTORS)}, not {self.actor!r}")

    @classmethod
    def of(cls, value: Provenance | Mapping[str, Any] | None) -> Provenance | None:
        """``value`` as a :class:`Provenance`: one already, or a mapping of its fields."""
        if value is None or isinstance(value, Provenance):
            return value
        if isinstance(value, Mapping):
            return cls(**dict(value))
        raise TypeError(f"provenance is a reef.Provenance or a mapping, not {type(value).__name__}")

    def with_env_defaults(self) -> Provenance:
        """This declaration with ``REEF_MODEL`` and ``REEF_TASK`` where it sets no model or task,
        as ``reef ws create`` applies them to a new workspace.
        """

        def env(name: str, ok: Callable[[str], bool]) -> str | None:
            value = os.environ.get(name, "").strip()
            # A value the daemon would refuse is ignored: the environment
            # never fails a creation.
            return value if value and ok(value) else None

        return Provenance(
            model=self.model if self.model is not None else env(MODEL_ENV, is_label),
            harness=self.harness,
            task=self.task if self.task is not None else env(TASK_ENV, is_task),
            instruction_sha256=self.instruction_sha256,
            client_session=self.client_session,
            identity=self.identity,
            actor=self.actor,
        )

    def is_empty(self) -> bool:
        """Whether it declares nothing."""
        return all(getattr(self, name) is None for name in (*_CONTEXT, "identity", "actor"))

    def message(self, *, creating: bool) -> ProvenanceDeclaration:
        """The ``ProvenanceDeclaration`` message. A new workspace has nothing to clear, so when
        ``creating`` an empty field declares nothing.
        """
        from reef.api.v1 import ProvenanceDeclaration, SessionActor

        if not creating and (self.identity is not None or self.actor is not None):
            raise TypeError(
                "identity and actor are fixed when the workspace is created; declare() takes "
                "the context fields only"
            )
        fields: dict[str, Any] = {}
        for name in _CONTEXT:
            value = getattr(self, name)
            if value is None or (creating and value == ""):
                continue
            fields[name] = value.strip() if name != "instruction_sha256" else value.lower()
        if self.identity:
            fields["identity"] = self.identity
        if self.actor is not None:
            fields["actor"] = SessionActor.Value(f"SESSION_ACTOR_{self.actor.upper()}")
        return ProvenanceDeclaration(**fields)


#: The reason a daemon that records no provenance refuses a declaration with.
PROVENANCE_DISABLED = "PROVENANCE_DISABLED"


@dataclass(frozen=True)
class CreatePlan:
    """What a new workspace declares: the caller's declaration with the environment's defaults,
    and whether the caller gave any of it (else a daemon that cannot take it is not an error).
    """

    declaration: Provenance | None
    explicit: bool

    @classmethod
    def of(cls, provenance: Provenance | Mapping[str, Any] | None) -> CreatePlan:
        """The plan for ``provenance``, with ``REEF_MODEL`` and ``REEF_TASK`` as defaults."""
        given = Provenance.of(provenance)
        declared = (given or Provenance()).with_env_defaults()
        if not declared.message(creating=True).ListFields():
            return cls(None, False)
        # Explicit when the caller's own fields declare something: an empty
        # field declares nothing on a new workspace.
        explicit = given is not None and bool(given.message(creating=True).ListFields())
        return cls(declared, explicit)


def declare_fields(
    *,
    model: str | None,
    harness: str | None,
    task: str | None,
    instruction: str | bytes | None,
    instruction_sha256: str | None,
    client_session: str | None,
) -> ProvenanceDeclaration:
    """``declare()``'s keyword arguments as the message."""
    return Provenance(
        model=model,
        harness=harness,
        task=task,
        instruction=instruction,
        instruction_sha256=instruction_sha256,
        client_session=client_session,
    ).message(creating=False)
