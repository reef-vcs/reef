"""The integration tests' harness: a scratch daemon, ``$REEF_BIN_DIR/reef-daemon`` (default
``${CARGO_TARGET_DIR:-<repo>/target}/debug``), on a state directory whose path has a space."""

from __future__ import annotations

import os
import signal
import socket
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

import pytest

import reef
from reef.api.v1 import RemoteListenerState

ROOT = Path(__file__).resolve().parents[4]


def bin_dir() -> Path:
    given = os.environ.get("REEF_BIN_DIR")
    if given:
        return Path(given)
    target = os.environ.get("CARGO_TARGET_DIR") or str(ROOT / "target")
    return Path(target) / "debug"


@dataclass
class Daemon:
    """A running scratch daemon."""

    root: Path
    process: subprocess.Popen[bytes] | None = None
    config: str = ""
    env: dict[str, str] = field(default_factory=dict)

    @property
    def state(self) -> Path:
        return self.root / "state dir"

    @property
    def socket(self) -> str:
        return str(self.state / "daemon.sock")

    def start(self) -> None:
        self.state.mkdir(mode=0o700, exist_ok=True)
        (self.root / "home").mkdir(exist_ok=True)
        (self.state / "config.toml").write_text(self.config)
        log = (self.root / "daemon.log").open("ab")
        self.process = subprocess.Popen(
            [str(bin_dir() / "reef-daemon"), "--foreground"],
            env=self.env,
            cwd=self.root,
            stdout=log,
            stderr=log,
        )
        log.close()
        deadline = time.monotonic() + 30
        while True:
            if self.process.poll() is not None:
                raise RuntimeError(f"reef-daemon exited during start:\n{self.log()}")
            try:
                client = reef.connect(state_dir=self.state, connect_timeout=0.5)
                break
            except reef.ReefError:
                if time.monotonic() > deadline:
                    raise RuntimeError(
                        f"reef-daemon did not answer in 30 s:\n{self.log()}"
                    ) from None
                time.sleep(0.1)
        # The socket answers before the remote listener binds (step 11 binds in the background):
        # until then nothing listens, and the status gives the configured address (port 0 here).
        with client:
            while True:
                if self.process.poll() is not None:
                    raise RuntimeError(f"reef-daemon exited during start:\n{self.log()}")
                remote = client.get_daemon_status(skip_backends=True).remote
                if remote.state in (
                    RemoteListenerState.REMOTE_LISTENER_STATE_OFF,
                    RemoteListenerState.REMOTE_LISTENER_STATE_LISTENING,
                ):
                    return
                if time.monotonic() > deadline:
                    raise RuntimeError(
                        f"the remote listener did not start: {remote.error}\n{self.log()}"
                    )
                time.sleep(0.1)

    def stop(self) -> None:
        process, self.process = self.process, None
        if process is None or process.poll() is not None:
            return
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(30)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    def log(self) -> str:
        try:
            return (self.root / "daemon.log").read_text(errors="replace")[-4000:]
        except OSError:
            return ""

    def reef(self, *args: str) -> str:
        """Runs the debug ``reef`` CLI against this daemon."""
        result = subprocess.run(
            [str(bin_dir() / "reef"), *args],
            env=self.env,
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            raise RuntimeError(f"reef {' '.join(args)}: {result.stderr}")
        return result.stdout


def daemon_env(root: Path) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith(("REEF_", "XDG_"))}
    env.update(
        REEF_STATE_DIR=str(root / "state dir"),
        REEF_LOG="info",
        HOME=str(root / "home"),
        GIT_CONFIG_GLOBAL="/dev/null",
        GIT_CONFIG_NOSYSTEM="1",
        GIT_AUTHOR_NAME="reef sdk",
        GIT_AUTHOR_EMAIL="sdk@example.com",
        GIT_COMMITTER_NAME="reef sdk",
        GIT_COMMITTER_EMAIL="sdk@example.com",
    )
    return env


def make_daemon(config: str) -> Daemon:
    if not (bin_dir() / "reef-daemon").exists():
        pytest.fail(
            f"no reef-daemon in {bin_dir()}: run `cargo build -p reef-vcs -p reef-daemon`, "
            "or set REEF_BIN_DIR"
        )
    root = Path(tempfile.mkdtemp(prefix="reef py "))
    daemon = Daemon(root=root, config=config, env=daemon_env(root))
    if len(os.fsencode(daemon.socket)) > 103:
        pytest.fail(f"{daemon.socket} is too long for a socket: set TMPDIR to a shorter directory")
    return daemon


BASE_CONFIG = '[watcher]\nbackend = "notify"\n\n[lease]\nheartbeat = "1s"\n'


def git(cwd: Path, env: dict[str, str], *args: str) -> None:
    subprocess.run(["git", *args], cwd=cwd, env=env, check=True, capture_output=True)


def make_repo(daemon: Daemon, name: str) -> Path:
    path = daemon.root / name
    path.mkdir()
    git(path, daemon.env, "init", "-q", "-b", "main")
    for n in range(3):
        (path / "README.md").write_text(f"The SDK's fixture, commit {n}.\n")
        git(path, daemon.env, "add", "README.md")
        git(path, daemon.env, "commit", "-q", "-m", f"commit {n}")
    return path


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])
