"""The installed wheel against a real daemon: what the release checks before publishing.

Run with a Python where only the built reef-vcs wheel is installed (not this checkout), and
``REEF_BIN_DIR`` naming a directory with ``reef-daemon``::

    REEF_BIN_DIR=target/debug python sdk/python/tests/smoke.py

Starts a scratch daemon, connects, checks the version and API revision, registers a fixture
repository, runs a workspace block with one edit, reads the edit back from the change, and stops
the daemon. Exits non-zero on any failure.
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def main() -> int:
    """Runs the smoke test."""
    import reef

    here = Path(__file__).resolve().parents[1] / "src"
    if Path(reef.__file__).resolve().is_relative_to(here):
        print(f"reef is imported from the checkout ({reef.__file__}), not a wheel", file=sys.stderr)
        return 2
    bin_dir = Path(os.environ["REEF_BIN_DIR"])
    root = Path(tempfile.mkdtemp(prefix="reef smoke "))
    state = root / "state"
    state.mkdir(mode=0o700)
    (state / "config.toml").write_text('[watcher]\nbackend = "notify"\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith("REEF_")}
    env.update(
        REEF_STATE_DIR=str(state),
        HOME=str(root),
        GIT_CONFIG_GLOBAL="/dev/null",
        GIT_AUTHOR_NAME="smoke",
        GIT_AUTHOR_EMAIL="smoke@example.com",
        GIT_COMMITTER_NAME="smoke",
        GIT_COMMITTER_EMAIL="smoke@example.com",
    )
    log = (root / "daemon.log").open("wb")
    daemon = subprocess.Popen(
        [str(bin_dir / "reef-daemon"), "--foreground"], env=env, stdout=log, stderr=log
    )
    try:
        deadline = time.monotonic() + 30
        while True:
            try:
                client = reef.connect(state_dir=state, connect_timeout=0.5)
                break
            except reef.DaemonNotRunningError:
                if time.monotonic() > deadline or daemon.poll() is not None:
                    raise
                time.sleep(0.1)
        with client:
            status = client.daemon_status()
            print(f"reef-vcs {reef.__version__} (API revision {reef.API_REVISION}) against "
                  f"reef {status.version} (API revision {status.api_revision})")  # fmt: skip
            assert status.api_revision == reef.API_REVISION, "API revisions differ"
            from reef._build import CARGO_VERSION

            assert status.version == CARGO_VERSION, "versions differ"

            fixture = root / "fixture"
            fixture.mkdir()
            for args in (
                ["init", "-q", "-b", "main"],
                ["commit", "-q", "--allow-empty", "-m", "fixture"],
            ):
                subprocess.run(["git", *args], cwd=fixture, env=env, check=True)
            repo = client.register_repository(
                path=str(fixture), if_not_jj="IF_NOT_JJ_INIT_ALONGSIDE_GIT"
            ).repository.id
            with client.workspace(repo, purpose="smoke") as ws:
                (ws.path / "smoke.txt").write_text("from the wheel\n")
                change = ws.change_id
            read = client.read_file(repo=repo, revision=change, path="smoke.txt")
            assert read.content == b"from the wheel\n", read.content
        print("smoke test passed")
        return 0
    finally:
        daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(30)
        except subprocess.TimeoutExpired:
            daemon.kill()
        log.close()
        if os.environ.get("REEF_SMOKE_KEEP"):
            print(f"kept {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
