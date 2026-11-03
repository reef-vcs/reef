"""How a client reaches the daemon: its socket, or a remote listener (R2.08), as grpc channels.

The socket's channel sets ``:authority`` to ``localhost``: grpc's default, the percent-encoded
socket path, is refused by the daemon's HTTP/2 server. A remote daemon is checked before grpc
connects, by a handshake of the SDK's own (``_tls``): against the pin, or against the same roots
grpc will trust. grpc then trusts only that.
"""

from __future__ import annotations

import datetime as dt
import os
import ssl
from dataclasses import dataclass, field
from typing import Any

import grpc

from reef import _tls
from reef._remote import RemoteTarget
from reef.errors import RemoteConnectError, RemoteTlsError

MAX_MESSAGE_BYTES = 4 << 20
"""The largest message a daemon decodes or sends: tonic's default, which the daemon keeps."""

KEEPALIVE_MS = 30_000

ROOTS_ENV = "GRPC_DEFAULT_SSL_ROOTS_FILE_PATH"


@dataclass
class Spec:
    """Everything needed to open a channel to one daemon."""

    target: str
    """grpc's target: ``unix:<path>`` or ``<host>:<port>``."""
    label: str
    """How errors name it: the socket path, or the remote's origin."""
    options: list[tuple[str, Any]]
    credentials: grpc.ChannelCredentials | None = None
    remote: RemoteTarget | None = None
    leaf: _tls.Leaf | None = field(default=None, repr=False)
    """The pinned remote's certificate grpc trusts."""
    root_certificates: bytes | None = field(default=None, repr=False)

    def open(self) -> grpc.Channel:
        """A sync channel."""
        if self.credentials is None:
            return grpc.insecure_channel(self.target, options=self.options)
        return grpc.secure_channel(self.target, self.credentials, options=self.options)

    def open_aio(self) -> grpc.aio.Channel:
        """An asyncio channel, on the running loop."""
        if self.credentials is None:
            return grpc.aio.insecure_channel(self.target, options=self.options)
        return grpc.aio.secure_channel(self.target, self.credentials, options=self.options)


def _limits() -> list[tuple[str, Any]]:
    return [
        ("grpc.max_send_message_length", MAX_MESSAGE_BYTES),
        ("grpc.max_receive_message_length", MAX_MESSAGE_BYTES),
    ]


def local(socket: str) -> Spec:
    """The daemon's socket."""
    return Spec(
        target=f"unix:{socket}",
        label=socket,
        options=[("grpc.default_authority", "localhost"), *_limits()],
    )


def _grpc_roots() -> bytes:
    """The roots grpc trusts by default: its environment override, else its bundled file."""
    path = os.environ.get(ROOTS_ENV) or os.path.join(
        os.path.dirname(grpc.__file__), "_cython", "_credentials", "roots.pem"
    )
    with open(path, "rb") as file:
        return file.read()


def _presented(url_host: str, der: bytes) -> str:
    del url_host
    try:
        return _tls.parse(der).fingerprint
    except _tls.DerError:
        return "an unparsable certificate"


def remote(
    target: RemoteTarget,
    *,
    connect_timeout: float,
    root_certificates: bytes | None = None,
) -> Spec:
    """A remote daemon, verified before anything is sent.

    Raises:
        RemoteConnectError: Nothing answered.
        RemoteTlsError: Its certificate was refused, or the handshake failed.
    """
    url = target.url
    origin = url.origin
    try:
        der = _tls.fetch_leaf(url, connect_timeout)
    except ssl.SSLError as error:
        raise RemoteTlsError(origin, "handshake", reason=str(error)) from error
    except OSError as error:
        raise RemoteConnectError(origin, str(error)) from error
    options: list[tuple[str, Any]] = [
        ("grpc.default_authority", url.authority),
        ("grpc.keepalive_time_ms", KEEPALIVE_MS),
        ("grpc.keepalive_permit_without_calls", 1),
        ("grpc.http2.max_pings_without_data", 0),
        *_limits(),
    ]
    call_credentials = grpc.access_token_call_credentials(target.token)
    if target.fingerprint:
        try:
            leaf = _tls.parse(der)
        except _tls.DerError as error:
            raise RemoteTlsError(origin, "handshake", reason=f"its certificate: {error}") from error
        if leaf.fingerprint != target.fingerprint:
            raise RemoteTlsError(
                origin,
                "pin_mismatch",
                presented=leaf.fingerprint,
                expected=target.fingerprint,
            )
        check_dates(leaf, origin, dt.datetime.now(dt.UTC))
        # grpc checks the certificate's names, and a call's :authority against them, even with
        # the key pinned. The pin is what authenticates the daemon, so when the URL's host is not
        # one of the names (an IP address the certificate does not list, say), the SDK names one
        # that is, as the daemon's own Host check accepts.
        name = _tls.target_name(leaf, url.host)
        if name is not None:
            options.append(("grpc.ssl_target_name_override", name))
            if name != url.host:
                options[0] = ("grpc.default_authority", _authority(name, url.port))
        credentials = grpc.composite_channel_credentials(
            grpc.ssl_channel_credentials(root_certificates=leaf.pem), call_credentials
        )
        return Spec(
            target=url.authority,
            label=origin,
            options=options,
            credentials=credentials,
            remote=target,
            leaf=leaf,
        )
    roots = root_certificates or _grpc_roots()
    context = ssl.create_default_context(cadata=roots.decode("ascii", "replace"))
    try:
        context.check_hostname = True
        import socket

        with (
            socket.create_connection((url.host, url.port), timeout=connect_timeout) as raw,
            context.wrap_socket(raw, server_hostname=url.host),
        ):
            pass
    except ssl.SSLCertVerificationError as error:
        raise RemoteTlsError(
            origin,
            "untrusted",
            presented=_presented(url.host, der),
            reason=error.verify_message or str(error),
        ) from error
    except ssl.SSLError as error:
        raise RemoteTlsError(origin, "handshake", reason=str(error)) from error
    except OSError as error:
        raise RemoteConnectError(origin, str(error)) from error
    credentials = grpc.composite_channel_credentials(
        grpc.ssl_channel_credentials(root_certificates=root_certificates), call_credentials
    )
    return Spec(
        target=url.authority,
        label=origin,
        options=options,
        credentials=credentials,
        remote=target,
        root_certificates=root_certificates,
    )


def _authority(host: str, port: int) -> str:
    return f"[{host}]:{port}" if ":" in host else f"{host}:{port}"


def check_dates(leaf: _tls.Leaf, origin: str, now: dt.datetime) -> None:
    """Raises :class:`RemoteTlsError` (``handshake``) for a pinned certificate outside its dates.

    grpc checks the dates even with the key pinned, which the pin does not need; a lapsed
    certificate would otherwise fail inside grpc as a connection that never comes up.
    """
    if leaf.not_after is not None and now > leaf.not_after:
        raise RemoteTlsError(
            origin,
            "handshake",
            reason=f"the daemon's certificate expired at {leaf.not_after:%Y-%m-%d %H:%M} UTC",
            hint=(
                "The daemon's certificate has lapsed. A generated one is renewed when the "
                "daemon starts: restart it on its machine. Renew a supplied one "
                "(`[remote] tls_cert`)."
            ),
        )
    if leaf.not_before is not None and now < leaf.not_before:
        raise RemoteTlsError(
            origin,
            "handshake",
            reason=f"the daemon's certificate is valid only from "
            f"{leaf.not_before:%Y-%m-%d %H:%M} UTC; check this machine's clock",
        )


def repinned(spec: Spec, connect_timeout: float) -> Spec | None:
    """For a pinned remote whose connection failed: a new spec if the daemon now presents a new
    certificate for the pinned key (a renewal), else None.

    Raises:
        RemoteTlsError: It now presents another key.
    """
    if spec.remote is None or spec.leaf is None or not spec.remote.fingerprint:
        return None
    try:
        der = _tls.fetch_leaf(spec.remote.url, connect_timeout)
    except (OSError, ssl.SSLError):
        return None
    if der == spec.leaf.der:
        return None
    return remote(spec.remote, connect_timeout=connect_timeout)
