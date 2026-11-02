"""Hatchling hooks for reef-vcs (R2.06, ADR 0054).

In a checkout of the repository, the build hook generates the gRPC modules and the clients'
methods from ``crates/reef-api/proto`` (``codegen/generate.py``), and the metadata hook takes the
version from the workspace's ``Cargo.toml``. Building the wheel from the sdist generates nothing:
the sdist carries the generated files, and the version is read from ``src/reef/_build.py``.
"""

from __future__ import annotations

import re
import shutil
import sys
from pathlib import Path
from typing import Any

from hatchling.builders.hooks.plugin.interface import BuildHookInterface
from hatchling.metadata.plugin.interface import MetadataHookInterface

HERE = Path(__file__).resolve().parent
BUILD_FILE = HERE / "src" / "reef" / "_build.py"


def _in_checkout() -> bool:
    return (HERE.parent.parent / "crates" / "reef-api" / "proto").is_dir()


def _generate() -> None:
    sys.path.insert(0, str(HERE / "codegen"))
    try:
        import generate
    finally:
        sys.path.pop(0)
    generate.main([])


def _version_from_build_file() -> str:
    if not BUILD_FILE.is_file():
        raise RuntimeError(
            f"{BUILD_FILE} is missing: build reef-vcs from a checkout of the reef repository, "
            "or from an sdist built there"
        )
    match = re.search(r'^VERSION = "([^"]+)"$', BUILD_FILE.read_text(), re.MULTILINE)
    if not match:
        raise RuntimeError(f"{BUILD_FILE} has no VERSION line")
    return match.group(1)


class GenerateHook(BuildHookInterface[Any]):
    """Generates the code before a build from a checkout."""

    PLUGIN_NAME = "custom"

    def initialize(self, version: str, build_data: dict[str, Any]) -> None:
        """Runs codegen/generate.py when the protos are beside this file."""
        if _in_checkout():
            _generate()
        else:
            _version_from_build_file()


class VersionHook(MetadataHookInterface):
    """Sets ``version`` from the workspace's Cargo.toml, or the generated _build.py."""

    PLUGIN_NAME = "custom"

    def update(self, metadata: dict[str, Any]) -> None:
        """Fills in the dynamic version."""
        if _in_checkout():
            # The license file is copied here, before hatchling resolves `license-files`.
            shutil.copyfile(HERE.parent.parent / "LICENSE", HERE / "LICENSE")
            sys.path.insert(0, str(HERE / "codegen"))
            try:
                import generate

                metadata["version"] = generate.pep440(generate.workspace_version())
            finally:
                sys.path.pop(0)
        else:
            metadata["version"] = _version_from_build_file()
