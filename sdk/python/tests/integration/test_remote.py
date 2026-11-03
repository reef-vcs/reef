"""The SDK against a daemon's remote listener (R2.08): pinned, by profile, CA-verified."""

from __future__ import annotations

import shutil
import subprocess
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from harness import BASE_CONFIG, Daemon, bin_dir, free_port, make_daemon, make_repo

import reef
import reef.aio
from reef.api.v1 import FileEdit, WorkspaceState

# LibreSSL (macOS /usr/bin/openssl) writes explicit curve parameters unless told
# otherwise, and OpenSSL 3 refuses to verify a chain with such a key.
P256 = ("-pkeyopt", "ec_paramgen_curve:P-256", "-pkeyopt", "ec_param_enc:named_curve")

# The test CA's own configuration, not the system's: OpenSSL 1.1.1 (Homebrew's openssl@1.1, first
# on PATH on the macOS runners) applies the system file's v3_ca section and appends `-addext`'s
# copy, and OpenSSL 3 refuses a CA with two basicConstraints.
CA_CONFIG = """[req]
distinguished_name = dn
prompt = no
x509_extensions = ca
[dn]
CN = reef test CA
[ca]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign
subjectKeyIdentifier = hash
"""

pytestmark = pytest.mark.remote


def remote_config(listen: str, extra: str = "") -> str:
    return (
        BASE_CONFIG
        + f'\n[remote]\nenabled = true\nlisten = "{listen}"\nnames = ["reefvm.test"]\n{extra}'
    )


class Remote:
    """A daemon with its remote listener, a repository, and write and read tokens."""

    def __init__(self, daemon: Daemon) -> None:
        self.daemon = daemon
        with reef.connect(state_dir=daemon.state) as admin:
            status = admin.get_daemon_status(skip_backends=True)
            self.address = status.remote.address
            self.fingerprint = status.remote.certificate.fingerprint
            path = make_repo(daemon, "remote repo")
            self.repo = admin.register_repository(
                path=str(path), if_not_jj="IF_NOT_JJ_INIT_ALONGSIDE_GIT"
            ).repository.id
            self.write = admin.create_remote_token(
                name="sdk-write", scope="REMOTE_SCOPE_WRITE", never_expires=True
            ).secret
            self.read = admin.create_remote_token(
                name="sdk-read", scope="REMOTE_SCOPE_READ", never_expires=True
            ).secret

    @property
    def url(self) -> str:
        return f"https://{self.address}"

    def token(self, name: str) -> Any:
        with reef.connect(state_dir=self.daemon.state) as admin:
            return next(t for t in admin.list_remote_tokens_all() if t.name == name)


@pytest.fixture(scope="module")
def remote() -> Iterator[Remote]:
    daemon = make_daemon(remote_config("127.0.0.1:0"))
    daemon.start()
    try:
        yield Remote(daemon)
    finally:
        daemon.stop()
        shutil.rmtree(daemon.root, ignore_errors=True)


@pytest.fixture
def no_remote_env(monkeypatch: pytest.MonkeyPatch) -> pytest.MonkeyPatch:
    for name in ("REEF_REMOTE", "REEF_REMOTE_TOKEN", "REEF_REMOTE_FINGERPRINT", "REEF_SOCKET"):
        monkeypatch.delenv(name, raising=False)
    return monkeypatch


def test_remote_pinned_by_environment(remote: Remote, no_remote_env: pytest.MonkeyPatch) -> None:
    no_remote_env.setenv("REEF_REMOTE", remote.url)
    no_remote_env.setenv("REEF_REMOTE_TOKEN", remote.write)
    no_remote_env.setenv("REEF_REMOTE_FINGERPRINT", remote.fingerprint)
    with reef.connect(agent="remote-sdk") as client:
        assert client.remote and client.target == remote.url
        assert client.daemon_status().api_revision == reef.API_REVISION
        list(client.list_workspaces_all(repo=remote.repo))
        with client.workspace(remote.repo, purpose="over the wire", report_presence=False) as ws:
            ws.edit_files(edits=[FileEdit(path="remote.txt", content=b"remote\n")])
            change = ws.change_id
        read = client.read_file(repo=remote.repo, revision=change, path="remote.txt")
        assert read.content == b"remote\n"
        ops = list(client.list_operations_all(repo=remote.repo, page_size=5))
        assert any(op.HasField("remote") for op in ops), "operations name the token"


async def test_remote_async(remote: Remote, no_remote_env: pytest.MonkeyPatch) -> None:
    client = await reef.aio.connect(
        remote=remote.url, token=remote.read, fingerprint=remote.fingerprint
    )
    async with client:
        status = await client.get_daemon_status(skip_backends=True)
        assert status.remote.certificate.fingerprint == remote.fingerprint


def test_remote_profile(remote: Remote, no_remote_env: pytest.MonkeyPatch, short_tmp: Path) -> None:
    state = short_tmp / "client"
    env = dict(remote.daemon.env, REEF_STATE_DIR=str(state))
    subprocess.run(
        [
            str(bin_dir() / "reef"),
            "remote",
            "add",
            "vm",
            remote.url,
            "--fingerprint",
            remote.fingerprint,
            "--token-file",
            "-",
        ],
        input=remote.read + "\n",
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    with reef.connect(remote="vm", state_dir=state) as client:
        assert client.target == remote.url
        client.get_daemon_status(skip_backends=True)


def test_remote_pin_mismatch_sends_no_token(
    remote: Remote, no_remote_env: pytest.MonkeyPatch
) -> None:
    with reef.connect(state_dir=remote.daemon.state) as admin:
        secret = admin.create_remote_token(
            name="sdk-unused", scope="REMOTE_SCOPE_READ", never_expires=True
        ).secret
    wrong = "sha256:" + "0" * 64
    with pytest.raises(reef.RemoteTlsError) as raised:
        reef.connect(remote=remote.url, token=secret, fingerprint=wrong)
    assert raised.value.failure == "pin_mismatch"
    assert raised.value.presented == remote.fingerprint
    assert remote.fingerprint in raised.value.hint
    assert not remote.token("sdk-unused").HasField("last_used_at"), "the token was never sent"


def test_remote_scopes(remote: Remote, no_remote_env: pytest.MonkeyPatch) -> None:
    with reef.connect(remote=remote.url, token=remote.read, fingerprint=remote.fingerprint) as c:
        c.list_workspaces(repo=remote.repo)
        with pytest.raises(reef.PermissionDeniedError) as raised:
            c.create_workspace(repo=remote.repo, owner="x@y")
        assert raised.value.reason
        with pytest.raises(reef.PermissionDeniedError):
            c.list_remote_tokens()
    with reef.connect(remote=remote.url, token="reef_wrong", fingerprint=remote.fingerprint) as c:
        with pytest.raises(reef.PermissionDeniedError) as raised:
            c.list_workspaces(repo=remote.repo)
        assert raised.value.reason == "REMOTE_TOKEN_INVALID"


def test_remote_host_not_in_the_certificate(
    remote: Remote, no_remote_env: pytest.MonkeyPatch
) -> None:
    # `localhost` is neither a name of the generated certificate nor in `[remote] names`: pinned,
    # the SDK names the certificate's own name to grpc and the daemon.
    port = remote.address.rsplit(":", 1)[1]
    url = f"https://localhost:{port}"
    with reef.connect(remote=url, token=remote.read, fingerprint=remote.fingerprint) as client:
        assert client.target == url
        client.get_daemon_status(skip_backends=True)


def openssl(*args: str, cwd: Path) -> None:
    subprocess.run(["openssl", *args], cwd=cwd, check=True, capture_output=True)


def self_signed(
    directory: Path, key: str, cert: str, days: str = "30", names: str = "DNS:reefvm.test"
) -> None:
    if not (directory / key).exists():
        openssl(
            "genpkey", "-algorithm", "EC", *P256, "-out", key,
            cwd=directory,
        )  # fmt: skip
    openssl(
        "req", "-new", "-x509", "-key", key, "-out", cert, "-days", days,
        "-subj", "/CN=reefvm.test", "-addext", f"subjectAltName={names},IP:127.0.0.1",
        cwd=directory,
    )  # fmt: skip


@pytest.mark.skipif(shutil.which("openssl") is None, reason="needs openssl")
def test_remote_ca_verified(no_remote_env: pytest.MonkeyPatch, short_tmp: Path) -> None:
    tls = short_tmp / "tls"
    tls.mkdir()
    (tls / "ca.cnf").write_text(CA_CONFIG)
    openssl(
        "req", "-new", "-x509", "-config", "ca.cnf", "-newkey", "ec", *P256, "-nodes",
        "-keyout", "ca.key", "-out", "ca.pem", "-days", "30", "-sha256",
        cwd=tls,
    )  # fmt: skip
    openssl(
        "req", "-new", "-newkey", "ec", *P256, "-nodes",
        "-keyout", "leaf.key", "-out", "leaf.csr", "-subj", "/CN=reefvm.test",
        cwd=tls,
    )  # fmt: skip
    (tls / "ext.cnf").write_text(
        "subjectAltName=DNS:reefvm.test,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n"
        # OpenSSL 1.1.1 and LibreSSL add no AKID, which Python 3.13+ (VERIFY_X509_STRICT) requires.
        "authorityKeyIdentifier=keyid\n"
    )
    openssl(
        "x509", "-req", "-in", "leaf.csr", "-CA", "ca.pem", "-CAkey", "ca.key",
        "-CAcreateserial", "-out", "leaf.pem", "-days", "30", "-sha256", "-extfile", "ext.cnf",
        cwd=tls,
    )  # fmt: skip
    extra = f'tls_cert = "{tls / "leaf.pem"}"\ntls_key = "{tls / "leaf.key"}"\n'
    daemon = make_daemon(remote_config("127.0.0.1:0", extra))
    daemon.start()
    try:
        remote = Remote(daemon)
        ca = (tls / "ca.pem").read_bytes()
        with reef.connect(remote=remote.url, token=remote.read, root_certificates=ca) as client:
            client.get_daemon_status(skip_backends=True)
        with pytest.raises(reef.RemoteTlsError) as raised:
            reef.connect(remote=remote.url, token=remote.read)
        assert raised.value.failure == "untrusted"
        assert raised.value.presented.startswith("sha256:")
    finally:
        daemon.stop()
        shutil.rmtree(daemon.root, ignore_errors=True)


@pytest.mark.skipif(shutil.which("openssl") is None, reason="needs openssl")
def test_remote_repins_after_renewal(no_remote_env: pytest.MonkeyPatch, short_tmp: Path) -> None:
    tls = short_tmp / "tls"
    tls.mkdir()
    self_signed(tls, "key.pem", "cert1.pem")
    port = free_port()
    extra = f'tls_cert = "{tls / "cert.pem"}"\ntls_key = "{tls / "key.pem"}"\n'
    shutil.copy(tls / "cert1.pem", tls / "cert.pem")
    daemon = make_daemon(remote_config(f"127.0.0.1:{port}", extra))
    daemon.start()
    try:
        remote = Remote(daemon)
        client = reef.connect(remote=remote.url, token=remote.write, fingerprint=remote.fingerprint)
        client.get_daemon_status(skip_backends=True)

        # A renewal: a new certificate for the same key. A stream's failure re-pins (the next
        # stream works) ...
        self_signed(tls, "key.pem", "cert2.pem", days="31")
        shutil.copy(tls / "cert2.pem", tls / "cert.pem")
        daemon.stop()
        daemon.start()
        with pytest.raises(reef.UnavailableError), client.subscribe_operations() as stream:
            next(iter(stream))
        with client.subscribe_operations() as stream:
            assert next(iter(stream)) is not None

        # ... and a read is retried once on a new channel.
        self_signed(tls, "key.pem", "cert4.pem", days="32")
        shutil.copy(tls / "cert4.pem", tls / "cert.pem")
        daemon.stop()
        daemon.start()
        assert (
            client.get_daemon_status(skip_backends=True).remote.certificate.fingerprint
            == remote.fingerprint
        )
        # A write after the renewal goes over the new channel.
        with client.workspace(remote.repo, report_presence=False) as ws:
            assert ws.workspace.state == WorkspaceState.WORKSPACE_STATE_ACTIVE

        # A new key: refused before anything is sent.
        (tls / "key.pem").unlink()
        self_signed(tls, "key.pem", "cert3.pem")
        shutil.copy(tls / "cert3.pem", tls / "cert.pem")
        daemon.stop()
        daemon.start()
        with pytest.raises(reef.RemoteTlsError) as raised:
            client.get_daemon_status(skip_backends=True)
        assert raised.value.failure == "pin_mismatch"
        client.close()
    finally:
        daemon.stop()
        shutil.rmtree(daemon.root, ignore_errors=True)


@pytest.mark.skipif(shutil.which("openssl") is None, reason="needs openssl")
def test_remote_misdirected(no_remote_env: pytest.MonkeyPatch, short_tmp: Path) -> None:
    # A certificate naming `localhost`, which `[remote] names` does not: the daemon answers 421.
    tls = short_tmp / "tls"
    tls.mkdir()
    self_signed(tls, "key.pem", "cert.pem", names="DNS:reefvm.test,DNS:localhost")
    extra = f'tls_cert = "{tls / "cert.pem"}"\ntls_key = "{tls / "key.pem"}"\n'
    daemon = make_daemon(remote_config("127.0.0.1:0", extra))
    daemon.start()
    try:
        remote = Remote(daemon)
        port = remote.address.rsplit(":", 1)[1]
        with (
            reef.connect(
                remote=f"https://localhost:{port}",
                token=remote.read,
                fingerprint=remote.fingerprint,
            ) as client,
            pytest.raises(reef.RemoteMisdirectedError) as raised,
        ):
            client.get_daemon_status(skip_backends=True)
        assert raised.value.host == "localhost"
    finally:
        daemon.stop()
        shutil.rmtree(daemon.root, ignore_errors=True)
