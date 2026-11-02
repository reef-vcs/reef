"""Pinning a remote daemon's key before anything is sent (R2.08's requirement 1, ADR 0054).

grpc cannot pin a public key, so the SDK reads the daemon's leaf certificate on a handshake of
its own that sends nothing after it, hashes the leaf's SubjectPublicKeyInfo, and compares it with
the pin. grpc is then given that certificate as its only trusted root, so the token reaches only
a server holding the pinned key. A minimal DER walk finds the key and the names; nothing beyond
the standard library is needed.
"""

from __future__ import annotations

import datetime as dt
import hashlib
import ipaddress
import socket
import ssl
from dataclasses import dataclass

from reef._remote import RemoteUrl


class DerError(ValueError):
    """The certificate does not parse."""


def _read(data: bytes, at: int) -> tuple[int, int, int]:
    """The element at ``at``: its tag, where its content starts, and where it ends."""
    if at + 2 > len(data):
        raise DerError("truncated")
    tag = data[at]
    length = data[at + 1]
    start = at + 2
    if length & 0x80:
        count = length & 0x7F
        if count == 0 or count > 4 or start + count > len(data):
            raise DerError("bad length")
        length = int.from_bytes(data[start : start + count], "big")
        start += count
    end = start + length
    if end > len(data):
        raise DerError("truncated")
    return tag, start, end


@dataclass(frozen=True)
class Leaf:
    """What the SDK reads from a certificate."""

    der: bytes
    spki: bytes
    names: tuple[str, ...]
    """The subjectAltName's DNS names and IP addresses, in order."""
    not_before: dt.datetime | None = None
    not_after: dt.datetime | None = None

    @property
    def fingerprint(self) -> str:
        """``sha256:`` and the hex SHA-256 of the SubjectPublicKeyInfo: what clients pin."""
        return "sha256:" + hashlib.sha256(self.spki).hexdigest()

    @property
    def pem(self) -> bytes:
        """The certificate as PEM, for grpc's root certificates."""
        return ssl.DER_cert_to_PEM_cert(self.der).encode()


SAN_OID = bytes.fromhex("551d11")  # 2.5.29.17


def parse(der: bytes) -> Leaf:
    """The key and names of a DER certificate.

    Raises:
        DerError: It is not a certificate this walk understands.
    """
    try:
        tag, start, _ = _read(der, 0)
        if tag != 0x30:
            raise DerError("not a certificate")
        tag, tbs_start, tbs_end = _read(der, start)  # TBSCertificate
        if tag != 0x30:
            raise DerError("no TBSCertificate")
        at = tbs_start
        tag, _, el_end = _read(der, at)
        if tag == 0xA0:  # [0] version
            at = el_end
        # serial, signature algorithm, issuer, validity, subject
        validity: tuple[dt.datetime | None, dt.datetime | None] = (None, None)
        for index in range(5):
            tag, content, el_end = _read(der, at)
            if index == 3:
                validity = _validity(der, content, el_end)
            at = el_end
        tag, spki_start, spki_end = _read(der, at)
        if tag != 0x30:
            raise DerError("no SubjectPublicKeyInfo")
        spki = der[at:spki_end]
        del spki_start
        at = spki_end
        names: tuple[str, ...] = ()
        while at < tbs_end:
            tag, content, el_end = _read(der, at)
            if tag == 0xA3:  # [3] extensions
                names = _san(der, content, el_end)
            at = el_end
        return Leaf(der=der, spki=spki, names=names, not_before=validity[0], not_after=validity[1])
    except IndexError as error:  # pragma: no cover - _read bounds every index
        raise DerError("truncated") from error


def _time(der: bytes, at: int) -> tuple[dt.datetime | None, int]:
    tag, start, end = _read(der, at)
    text = der[start:end].decode("ascii", "replace")
    try:
        if tag == 0x17:  # UTCTime, YYMMDDHHMMSSZ: 50 to 99 are 19xx (RFC 5280)
            year = int(text[:2])
            text = ("19" if year >= 50 else "20") + text
        elif tag != 0x18:  # GeneralizedTime, YYYYMMDDHHMMSSZ
            return None, end
        return dt.datetime.strptime(text, "%Y%m%d%H%M%SZ").replace(tzinfo=dt.UTC), end
    except ValueError:
        return None, end


def _validity(der: bytes, start: int, end: int) -> tuple[dt.datetime | None, dt.datetime | None]:
    not_before, at = _time(der, start)
    not_after, _ = _time(der, at) if at < end else (None, at)
    return not_before, not_after


def _san(der: bytes, start: int, end: int) -> tuple[str, ...]:
    tag, at, seq_end = _read(der, start)
    if tag != 0x30 or seq_end > end:
        raise DerError("bad extensions")
    while at < seq_end:
        _, ext_at, ext_end = _read(der, at)
        tag, oid_start, oid_end = _read(der, ext_at)
        if tag == 0x06 and der[oid_start:oid_end] == SAN_OID:
            inner = oid_end
            tag, _, inner_end = _read(der, inner)
            if tag == 0x01:  # critical
                inner = inner_end
            tag, value_start, _ = _read(der, inner)
            if tag != 0x04:
                raise DerError("bad subjectAltName")
            tag, name_at, names_end = _read(der, value_start)
            names: list[str] = []
            while name_at < names_end:
                tag, n_start, n_end = _read(der, name_at)
                raw = der[n_start:n_end]
                if tag == 0x82:  # dNSName
                    names.append(raw.decode("ascii", "replace"))
                elif tag == 0x87 and len(raw) in (4, 16):  # iPAddress
                    names.append(str(ipaddress.ip_address(raw)))
                name_at = n_end
            return tuple(names)
        at = ext_end
    return ()


def fetch_leaf(url: RemoteUrl, timeout: float) -> bytes:
    """The leaf certificate the daemon at ``url`` presents, by a handshake that sends nothing.

    Raises:
        OSError: Nothing answered, or the connection failed before TLS.
        ssl.SSLError: The TLS handshake failed.
    """
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.set_alpn_protocols(["h2"])
    server_name: str | None = url.host
    try:
        ipaddress.ip_address(url.host)
        server_name = None
    except ValueError:
        pass
    with (
        socket.create_connection((url.host, url.port), timeout=timeout) as raw,
        context.wrap_socket(raw, server_hostname=server_name) as tls,
    ):
        der = tls.getpeercert(binary_form=True)
    if not der:
        raise ssl.SSLError("the daemon presented no certificate")
    return der


def target_name(leaf: Leaf, host: str) -> str | None:
    """The name grpc checks the certificate for: the URL's host if the leaf names it, else the
    leaf's first name, else None (grpc then checks the URL's host).
    """
    if host in leaf.names:
        return host
    return leaf.names[0] if leaf.names else None
