"""An executor written against the SDK (R3.05 test 17): the executor protocol's
lifecycle from Python. It sends no evidence: the SDK cannot sign yet, so the
signed half of the protocol is tested in Rust (crates/reef-daemon/tests/executor.rs).
"""

from __future__ import annotations

import json
import threading
import time
from datetime import timedelta

import pytest
from harness import Daemon

import reef
import reef._stream
from reef.api.v1 import (
    ExecutorInfo,
    JobOutcome,
    JobProgress,
    JobState,
    PresenceKind,
)


def wait_for(condition, timeout: float = 5.0, message: str = "condition") -> None:  # type: ignore[no-untyped-def]
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {message}")
        time.sleep(0.05)


def events(stream: reef._stream.Stream, seen: list) -> None:  # type: ignore[type-arg]
    for frame in stream:
        seen.extend(frame.events)


def claim(client: reef.Client) -> tuple[str, str]:
    response = client.claim_job(
        executor=ExecutorInfo(name="py-runner", version="0.1", kinds=["test"]),
        protocol_version=1,
        wait=timedelta(seconds=10),
    )
    assert response.protocol_version == 1
    assert response.job.job_id, "a job within the wait"
    return response.job.job_id, response.claim_token


def submit(daemon: Daemon, workspace: str) -> str:
    out = daemon.reef(
        "-o", "json", "job", "submit", "--workspace", workspace, "--kind", "test", "--duplicate"
    )
    job_id: str = json.loads(out)["job"]["id"]
    return job_id


def test_an_sdk_executor_runs_the_lifecycle(client: reef.Client, daemon: Daemon, repo: str) -> None:
    with client.workspace(repo, purpose="executor", report_presence=False) as ws:
        seen: list = []  # type: ignore[type-arg]
        stream = client.subscribe_presence(repos=[repo], kinds=[PresenceKind.PRESENCE_KIND_JOB])
        watcher = threading.Thread(target=events, args=(stream, seen), daemon=True)
        watcher.start()

        job_id = submit(daemon, ws.id)
        claimed, token = claim(client)
        assert claimed == job_id

        # Progress reaches a presence subscriber.
        client.update_job(
            job_id=job_id,
            claim_token=token,
            progress=JobProgress(phase="build", done=1, total=3, unit="steps"),
        )
        wait_for(
            lambda: any(
                e.presence.job.job_id == job_id and e.presence.job.progress.phase == "build"
                for e in seen
            ),
            message="the progress report",
        )

        # Released, claimed again, completed FAILED with a message.
        client.update_job(job_id=job_id, claim_token=token, release=True)
        assert client.get_job(job_id=job_id).job.state == JobState.JOB_STATE_QUEUED
        again, token = claim(client)
        assert again == job_id
        done = client.complete_job(
            job_id=job_id,
            claim_token=token,
            outcome=JobOutcome.JOB_OUTCOME_FAILED,
            message="the runner had no toolchain",
        ).job
        assert done.state == JobState.JOB_STATE_COMPLETED
        assert done.outcome == JobOutcome.JOB_OUTCOME_FAILED
        assert done.attempt == 2
        assert done.message == "the runner had no toolchain"
        wait_for(
            lambda: any(e.ended and e.presence.job.job_id == job_id for e in seen),
            message="the job's entry to end",
        )

        # A job cancelled while claimed: the executor's next update is refused.
        second = submit(daemon, ws.id)
        claimed, token = claim(client)
        assert claimed == second
        client.cancel_job(job_id=second, reason="superseded")
        with pytest.raises(reef.ConflictError) as raised:
            client.update_job(job_id=second, claim_token=token)
        assert raised.value.reason == "JOB_NOT_CLAIMED"
        assert raised.value.metadata["state"] == "cancelled"
        stream.cancel()
