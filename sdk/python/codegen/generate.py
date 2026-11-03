"""Generates reef-vcs's code from the schema (R2.06, ADR 0054).

Run by the build hook in a checkout, and by ``just python-gen`` for an editor. Writes, under
``src/reef/`` (all VCS-ignored):

- ``api/v1/``: protoc's messages and gRPC stubs for every ``crates/reef-api/proto/reef/api/v1``
  file, typed by mypy-protobuf, and an ``__init__`` re-exporting every message and enum by name;
- ``daemon/v0/``: 0.1's frozen status messages, for the incompatible-daemon probe;
- ``_rpcs.py``, ``_methods.py`` and ``aio/_methods.py``: the clients' methods (``clientgen.py``);
- ``_build.py``: the version, the API revision and the commit;

and copies the repository's LICENSE beside ``pyproject.toml``.
"""

from __future__ import annotations

import importlib.resources
import re
import shutil
import subprocess
import sys
import sysconfig
import tempfile
import tomllib
from pathlib import Path

from google.protobuf import descriptor_pb2

HERE = Path(__file__).resolve().parent
SDK = HERE.parent
ROOT = SDK.parent.parent
PROTO = ROOT / "crates" / "reef-api" / "proto"
OUT = SDK / "src" / "reef"
V1 = "reef/api/v1"
V0 = ["reef/daemon/v0/daemon.proto", "reef/daemon/v0/error.proto"]

sys.path.insert(0, str(HERE))
import clientgen  # noqa: E402


def workspace_version() -> str:
    """``[workspace.package] version`` from the repository's Cargo.toml."""
    with (ROOT / "Cargo.toml").open("rb") as file:
        version = tomllib.load(file)["workspace"]["package"]["version"]
    if not isinstance(version, str):
        raise RuntimeError("Cargo.toml's [workspace.package] version is not a string")
    return version


def pep440(version: str) -> str:
    """The Cargo version in PEP 440's normal form: ``0.4.0-rc.1`` is ``0.4.0rc1``."""
    from packaging.version import Version  # hatchling's dependency, so in every build env

    return str(Version(version))


def api_revision() -> int:
    """``reef_api::API_REVISION``, read from reef-api's lib.rs."""
    text = (ROOT / "crates" / "reef-api" / "src" / "lib.rs").read_text()
    match = re.search(r"^pub const API_REVISION: u32 = (\d+);$", text, re.MULTILINE)
    if not match:
        raise RuntimeError(
            "no `pub const API_REVISION: u32 = N;` line in crates/reef-api/src/lib.rs; "
            "update codegen/generate.py's pattern"
        )
    return int(match.group(1))


def commit() -> str:
    """The checkout's short commit id, or empty when neither git nor jj can tell."""
    for command in (
        ["git", "rev-parse", "--short=12", "HEAD"],
        ["jj", "log", "--no-graph", "-r", "@", "-T", "commit_id.short(12)"],
    ):
        try:
            result = subprocess.run(
                command, cwd=ROOT, capture_output=True, text=True, timeout=10, check=False
            )
        except (OSError, subprocess.TimeoutExpired):
            continue
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip()
    return ""


def v1_files() -> list[str]:
    """Every v1 ``.proto``, relative to the include root, sorted."""
    files = sorted(f"{V1}/{path.name}" for path in (PROTO / V1).glob("*.proto"))
    if not files:
        raise RuntimeError(f"no .proto files in {PROTO / V1}")
    return files


def _plugin(name: str) -> str:
    """The path of a protoc plugin installed beside this interpreter."""
    for directory in (Path(sysconfig.get_path("scripts")), Path(sys.executable).parent):
        candidate = directory / name
        if candidate.exists():
            return str(candidate)
    found = shutil.which(name)
    if found:
        return found
    raise RuntimeError(f"{name} is not installed; it comes with mypy-protobuf")


def protoc(args: list[str]) -> None:
    """Runs grpcio-tools' bundled protoc with the well-known types on the include path."""
    from grpc_tools import protoc as grpc_protoc

    well_known = importlib.resources.files("grpc_tools") / "_proto"
    command = [
        "protoc",
        f"-I{PROTO}",
        f"-I{well_known}",
        f"--plugin=protoc-gen-mypy={_plugin('protoc-gen-mypy')}",
        f"--plugin=protoc-gen-mypy_grpc={_plugin('protoc-gen-mypy_grpc')}",
        *args,
    ]
    if grpc_protoc.main(command) != 0:
        raise RuntimeError(f"protoc failed: {' '.join(command[1:])}")


def reexports(descriptors: descriptor_pb2.FileDescriptorSet) -> str:
    """``reef/api/v1/__init__.py``: every message and enum, by name, from its module."""
    lines = [
        '"""The reef.api.v1 schema: every message and enum, by name. Generated; do not edit."""',
        "",
    ]
    names: list[str] = []
    for file in descriptors.file:
        if not file.name.startswith(f"{V1}/"):
            continue
        module = Path(file.name).stem + "_pb2"
        exported = [m.name for m in file.message_type] + [e.name for e in file.enum_type]
        if not exported:
            continue
        lines.append(f"from reef.api.v1.{module} import (")
        lines.extend(f"    {name} as {name}," for name in sorted(exported))
        lines.append(")")
        names.extend(exported)
    duplicates = {name for name in names if names.count(name) > 1}
    if duplicates:
        raise RuntimeError(f"names defined twice in reef.api.v1: {sorted(duplicates)}")
    lines.append("")
    return "\n".join(lines)


def build_file(cargo_version: str, revision: int, commit_id: str) -> str:
    """``reef/_build.py``."""
    return (
        '"""What this build of reef-vcs was generated from. Generated; do not edit."""\n'
        "\n"
        f'VERSION = "{pep440(cargo_version)}"\n'
        f'CARGO_VERSION = "{cargo_version}"\n'
        f"API_REVISION = {revision}\n"
        f'COMMIT = "{commit_id}"\n'
    )


def main(argv: list[str]) -> int:
    """Generates everything; ``argv`` is unused."""
    del argv
    for generated in (OUT / "api", OUT / "daemon"):
        shutil.rmtree(generated, ignore_errors=True)
    out = str(OUT.parent)
    files = v1_files()
    protoc(
        [
            f"--python_out={out}",
            f"--grpc_python_out={out}",
            f"--mypy_out=quiet:{out}",
            f"--mypy_grpc_out=quiet:{out}",
            *files,
            *V0,
        ]
    )
    with tempfile.TemporaryDirectory() as scratch:
        fds = Path(scratch) / "reef_api_v1.fds"
        protoc(
            [
                f"--descriptor_set_out={fds}",
                "--include_source_info",
                "--include_imports",
                *files,
            ]
        )
        descriptors = descriptor_pb2.FileDescriptorSet.FromString(fds.read_bytes())

    for package in ("api", "api/v1", "daemon", "daemon/v0"):
        (OUT / package).mkdir(parents=True, exist_ok=True)
    (OUT / "api" / "__init__.py").write_text('"""Generated from the schema; do not edit."""\n')
    (OUT / "api" / "v1" / "__init__.py").write_text(reexports(descriptors))
    (OUT / "daemon" / "__init__.py").write_text('"""Generated from the schema; do not edit."""\n')
    (OUT / "daemon" / "v0" / "__init__.py").write_text(
        '"""0.1\'s frozen status messages, for the incompatible-daemon probe."""\n'
    )
    for package in ("api", "daemon"):
        (OUT / package / "py.typed").write_text("")

    rpcs, sync_methods, async_methods = clientgen.generate(descriptors)
    (OUT / "_rpcs.py").write_text(rpcs)
    (OUT / "_methods.py").write_text(sync_methods)
    (OUT / "aio" / "_methods.py").write_text(async_methods)
    (OUT / "_build.py").write_text(build_file(workspace_version(), api_revision(), commit()))
    shutil.copyfile(ROOT / "LICENSE", SDK / "LICENSE")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
