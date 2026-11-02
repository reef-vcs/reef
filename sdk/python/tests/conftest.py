"""Shared fixtures."""

from __future__ import annotations

import tempfile
from collections.abc import Iterator
from pathlib import Path

import pytest


@pytest.fixture
def short_tmp() -> Iterator[Path]:
    """A scratch directory with a short path: a socket path has at most 103 bytes, and pytest's
    tmp_path on macOS is longer than that leaves room for."""
    with tempfile.TemporaryDirectory(prefix="reef-") as path:
        yield Path(path)
