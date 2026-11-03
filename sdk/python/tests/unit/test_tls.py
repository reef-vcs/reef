"""The DER walk that finds a certificate's key and names, checked against openssl."""

from __future__ import annotations

import hashlib
import shutil
import subprocess
from pathlib import Path

import pytest

import reef
from reef import _tls

pytestmark = pytest.mark.skipif(shutil.which("openssl") is None, reason="needs openssl")


def make_cert(directory: Path, *, san: str | None, ca: bool = False) -> bytes:
    key = directory / "key.pem"
    cert = directory / "cert.der"
    command = [
        "openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
        "-nodes", "-keyout", str(key), "-out", str(directory / "cert.pem"), "-days", "30",
        "-subj", "/CN=reef test",
    ]  # fmt: skip
    if san:
        command += ["-addext", f"subjectAltName={san}"]
    if ca:
        command += ["-addext", "basicConstraints=critical,CA:TRUE"]
    subprocess.run(command, check=True, capture_output=True)
    subprocess.run(
        [
            "openssl",
            "x509",
            "-in",
            str(directory / "cert.pem"),
            "-outform",
            "DER",
            "-out",
            str(cert),
        ],
        check=True,
        capture_output=True,
    )
    return cert.read_bytes()


def openssl_fingerprint(directory: Path) -> str:
    pubkey = subprocess.run(
        ["openssl", "x509", "-in", str(directory / "cert.pem"), "-pubkey", "-noout"],
        check=True,
        capture_output=True,
    ).stdout
    der = subprocess.run(
        ["openssl", "pkey", "-pubin", "-outform", "DER"],
        input=pubkey,
        check=True,
        capture_output=True,
    ).stdout
    return "sha256:" + hashlib.sha256(der).hexdigest()


def test_der_walk(tmp_path: Path) -> None:
    der = make_cert(tmp_path, san="DNS:reefvm.test,IP:127.0.0.1,IP:::1")
    leaf = _tls.parse(der)
    assert leaf.fingerprint == openssl_fingerprint(tmp_path)
    assert leaf.names == ("reefvm.test", "127.0.0.1", "::1")
    assert _tls.target_name(leaf, "127.0.0.1") == "127.0.0.1"
    assert _tls.target_name(leaf, "other.example") == "reefvm.test"
    assert leaf.pem.startswith(b"-----BEGIN CERTIFICATE-----")


def test_der_walk_with_other_extensions(tmp_path: Path) -> None:
    der = make_cert(tmp_path, san="DNS:a.example", ca=True)
    leaf = _tls.parse(der)
    assert leaf.fingerprint == openssl_fingerprint(tmp_path)
    assert leaf.names == ("a.example",)


def test_der_walk_without_names(tmp_path: Path) -> None:
    der = make_cert(tmp_path, san=None)
    leaf = _tls.parse(der)
    assert leaf.names == ()
    assert _tls.target_name(leaf, "x") is None


def test_der_walk_refuses_garbage(tmp_path: Path) -> None:
    der = make_cert(tmp_path, san="DNS:a.example")
    for bad in (b"", b"not a certificate", der[:40], der[:-30], b"\x30\x84\xff\xff\xff\xff"):
        with pytest.raises(_tls.DerError):
            _tls.parse(bad)


def test_validity_dates(tmp_path: Path) -> None:
    import datetime as dt

    from reef import _channel

    leaf = _tls.parse(make_cert(tmp_path, san="DNS:a.example"))
    assert leaf.not_before is not None and leaf.not_after is not None
    now = dt.datetime.now(dt.UTC)
    assert leaf.not_before <= now < leaf.not_after
    assert dt.timedelta(days=29) < leaf.not_after - now < dt.timedelta(days=31)
    _channel.check_dates(leaf, "https://vm:1", now)
    with pytest.raises(reef.RemoteTlsError) as raised:
        _channel.check_dates(leaf, "https://vm:1", now + dt.timedelta(days=40))
    assert raised.value.failure == "handshake" and "lapsed" in raised.value.hint
    with pytest.raises(reef.RemoteTlsError, match="refused"):
        _channel.check_dates(leaf, "https://vm:1", now - dt.timedelta(days=1))
