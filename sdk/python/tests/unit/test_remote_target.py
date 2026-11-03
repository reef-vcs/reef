"""Remote URLs and target selection, as reef-client and the CLI have them (R2.08)."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import reef
from reef import _remote
from reef.errors import ConfigError, ErrorKind

FIXTURES = Path(__file__).resolve().parents[3] / "fixtures"
CASES = json.loads((FIXTURES / "remote-urls.json").read_text())["cases"]
PIN = "sha256:" + "ab" * 32


@pytest.mark.parametrize("case", CASES, ids=[case["url"] for case in CASES])
def test_remote_urls_follow_the_shared_table(case: dict) -> None:
    url = _remote.RemoteUrl.parse(case["url"])
    if case.get("valid") is False:
        assert url is None
        return
    assert url is not None
    assert (url.host, url.port, url.authority) == (case["host"], case["port"], case["authority"])
    assert url.origin == f"https://{case['authority']}"


def test_fingerprints() -> None:
    assert _remote.is_fingerprint(PIN)
    assert not _remote.is_fingerprint("sha256:" + "AB" * 32)
    assert not _remote.is_fingerprint("sha256:abc")


def write_profiles(state: Path) -> None:
    state.mkdir(parents=True, exist_ok=True)
    (state / "remotes.toml").write_text(
        '[vm]\nurl = "https://vm.example:7470"\n'
        f'fingerprint = "{PIN}"\ntoken = "reef_profile"\n'
        '[ca]\nurl = "https://ca.example"\ntoken = "reef_ca"\n'
    )


def test_remote_target_selection(tmp_path: Path) -> None:
    write_profiles(tmp_path)
    env: dict[str, str] = {}
    target = _remote.choose("vm", state_dir=str(tmp_path), env=env.get)
    assert (target.url.host, target.url.port, target.token, target.fingerprint) == (
        "vm.example",
        7470,
        "reef_profile",
        PIN,
    )
    assert target.label == "vm"
    assert "reef_profile" not in repr(target)

    # The environment overrides a profile's token and pin; arguments override both.
    env = {"REEF_REMOTE_TOKEN": " reef_env \n", "REEF_REMOTE_FINGERPRINT": "sha256:" + "cd" * 32}
    target = _remote.choose("vm", state_dir=str(tmp_path), env=env.get)
    assert (target.token, target.fingerprint) == ("reef_env", "sha256:" + "cd" * 32)
    target = _remote.choose(
        "vm", state_dir=str(tmp_path), token="reef_arg", fingerprint=PIN, env=env.get
    )
    assert (target.token, target.fingerprint) == ("reef_arg", PIN)

    target = _remote.choose("ca", state_dir=str(tmp_path), env={}.get)
    assert target.fingerprint is None

    # A URL needs a token.
    with pytest.raises(ConfigError) as raised:
        _remote.choose("https://vm:7470", state_dir=str(tmp_path), env={}.get)
    assert "REEF_REMOTE_TOKEN" in raised.value.hint
    target = _remote.choose(
        "https://vm:7470", state_dir=str(tmp_path), env={"REEF_REMOTE_TOKEN": "t"}.get
    )
    assert (target.url.authority, target.token, target.name) == ("vm:7470", "t", None)

    with pytest.raises(ConfigError) as raised:
        _remote.choose("https://vm/path", state_dir=str(tmp_path), env={}.get)

    with pytest.raises(ConfigError) as raised:
        _remote.choose("nope", state_dir=str(tmp_path), env={}.get)
    assert raised.value.kind == ErrorKind.NOT_FOUND
    assert raised.value.next == ["reef", "remote", "list"]


def test_profiles_file_errors(tmp_path: Path) -> None:
    assert _remote.load_profiles(str(tmp_path)) == {}
    (tmp_path / "remotes.toml").write_text("[vm\n")
    with pytest.raises(ConfigError):
        _remote.load_profiles(str(tmp_path))
    (tmp_path / "remotes.toml").write_text('[vm]\nurl = "https://vm"\ntoken = "t"\nextra = 1\n')
    with pytest.raises(ConfigError):
        _remote.load_profiles(str(tmp_path))


def test_socket_and_remote_together() -> None:
    with pytest.raises(TypeError):
        reef.connect("/tmp/x.sock", remote="vm")


def test_misdirected_matches_the_status_line_only() -> None:
    from reef import _connect

    assert _connect.MISDIRECTED.search("Received http2 header with status: 421")
    assert _connect.MISDIRECTED.search("protocol error: received HTTP status code 421")
    assert not _connect.MISDIRECTED.search("workspace 4210abcd not found")
    assert not _connect.MISDIRECTED.search("Received http2 header with status: 4211")
