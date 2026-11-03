"""Socket discovery, against the table reef-client passes too (sdk/fixtures/socket-paths.json)."""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from reef import _paths
from reef.errors import ConfigError

FIXTURES = Path(__file__).resolve().parents[3] / "fixtures"
CASES = json.loads((FIXTURES / "socket-paths.json").read_text())["cases"]


def fill(text: str, tmp: Path) -> str:
    return text.replace("{tmp}", str(tmp)).replace("{cwd}", os.getcwd())


@pytest.mark.parametrize("case", CASES, ids=[case["name"] for case in CASES])
def test_socket_paths_follow_the_shared_table(case: dict, short_tmp: Path) -> None:
    for path, text in (case.get("files") or {}).items():
        target = Path(fill(path, short_tmp))
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(fill(text, short_tmp))
    env = {key: fill(value, short_tmp) for key, value in (case.get("env") or {}).items()}
    args = {key: fill(value, short_tmp) for key, value in (case.get("args") or {}).items()}
    defaults = case.get("defaults")

    def platform() -> _paths.PlatformDefaults:
        if defaults is None:
            raise ConfigError("cannot find the home directory")
        runtime = defaults.get("runtime_dir")
        return _paths.PlatformDefaults(
            state_dir=fill(defaults["state_dir"], short_tmp),
            runtime_dir=fill(runtime, short_tmp) if runtime else None,
        )

    def settle() -> _paths.Settled:
        return _paths.settle(
            socket=args.get("socket"),
            state_dir=args.get("state_dir"),
            config=args.get("config"),
            env=env,
            defaults=platform,
        )

    if "error" in case:
        with pytest.raises(ConfigError) as raised:
            settle()
        assert raised.value.kind.value == case["error"]
        return
    settled = settle()
    assert settled.socket == fill(case["socket"], short_tmp)
    if "state_dir" in case:
        assert settled.state_dir == fill(case["state_dir"], short_tmp)
    if "config_file" in case:
        assert settled.config_file == fill(case["config_file"], short_tmp)


def test_platform_defaults() -> None:
    env = {"XDG_STATE_HOME": "/x/state", "XDG_RUNTIME_DIR": "/run/user/7"}
    linux = _paths.platform_defaults(env.get, home="/home/u", platform="linux")
    assert linux == _paths.PlatformDefaults("/x/state/reef", "/run/user/7")
    # etcetera ignores relative XDG variables, as the daemon does.
    relative = {"XDG_STATE_HOME": "state", "XDG_RUNTIME_DIR": ""}
    linux = _paths.platform_defaults(relative.get, home="/home/u", platform="linux")
    assert linux == _paths.PlatformDefaults("/home/u/.local/state/reef", None)
    mac = _paths.platform_defaults(env.get, home="/Users/u", platform="darwin")
    assert mac == _paths.PlatformDefaults("/Users/u/Library/Application Support/reef", None)


def test_settle_reads_the_real_environment(
    short_tmp: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("REEF_STATE_DIR", str(short_tmp / "with space"))
    monkeypatch.delenv("REEF_SOCKET", raising=False)
    monkeypatch.delenv("REEF_CONFIG", raising=False)
    assert _paths.settle().socket == str(short_tmp / "with space" / "daemon.sock")
    assert _paths.settle_state_dir() == str(short_tmp / "with space")
