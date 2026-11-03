"""Fixtures for the integration tests; the harness is harness.py."""

from __future__ import annotations

import shutil
from collections.abc import Iterator
from pathlib import Path

import pytest
from harness import BASE_CONFIG, Daemon, make_daemon, make_repo

import reef


def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    here = Path(__file__).parent
    for item in items:
        if Path(str(item.fspath)).is_relative_to(here):
            item.add_marker(pytest.mark.integration)


@pytest.fixture(scope="session")
def daemon() -> Iterator[Daemon]:
    daemon = make_daemon(BASE_CONFIG)
    daemon.start()
    try:
        yield daemon
    finally:
        daemon.stop()
        shutil.rmtree(daemon.root, ignore_errors=True)


@pytest.fixture(scope="session")
def repo(daemon: Daemon) -> str:
    """A git repository with three commits, registered through the SDK; its id."""
    path = make_repo(daemon, "fixture repo")
    with reef.connect(state_dir=daemon.state) as client:
        return client.register_repository(
            path=str(path), if_not_jj="IF_NOT_JJ_INIT_ALONGSIDE_GIT"
        ).repository.id


@pytest.fixture
def client(daemon: Daemon) -> Iterator[reef.Client]:
    with reef.connect(state_dir=daemon.state, agent="sdk-test") as client:
        yield client
