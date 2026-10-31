//! Starting, retrying and reporting the remote listener (R2.08): step 11
//! of the start sequence.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reef_api::v1;
use reef_lib::error::ErrorChain;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use super::failures::Failures;
use super::gate::Gate;
use super::tls::{CertInfo, Resolver, Source};
use super::tokens::Tokens;
use super::{Limits, RemoteError};
use crate::config::Config;
use crate::daemon::{AppState, SHUTDOWN_GRACE};
use crate::db::Db;

/// What turned the listener on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnabledBy {
    /// `[remote] enabled = true`.
    Config,
    /// `--remote`.
    Flag,
}

#[derive(Debug, Default)]
struct Status {
    /// The bound address once listening.
    bound: Option<SocketAddr>,
    /// Why it is not listening.
    error: Option<String>,
    resolver: Option<Arc<Resolver>>,
}

/// The remote listener and its tokens; `AppState::remote`.
#[derive(Debug)]
pub struct RemoteListener {
    enabled_by: Option<EnabledBy>,
    /// `[remote] names`, or the host name.
    names: Vec<String>,
    tokens: Arc<Tokens>,
    status: Mutex<Status>,
}

impl RemoteListener {
    /// The listener `config` asks for, not yet started, with the tokens in
    /// `db`. The tokens are loaded whether or not the listener is on, so
    /// they can be issued before it is.
    pub fn new(config: &Config, db: Arc<Db>) -> Result<RemoteListener, RemoteError> {
        let remote = &config.file.remote;
        let enabled_by = if config.remote_flag {
            Some(EnabledBy::Flag)
        } else if remote.enabled {
            Some(EnabledBy::Config)
        } else {
            None
        };
        let names = if remote.names.is_empty() {
            vec![config.operation.hostname.to_lowercase()]
        } else {
            remote
                .names
                .iter()
                .map(|name| name.to_lowercase())
                .collect()
        };
        Ok(RemoteListener {
            enabled_by,
            names,
            tokens: Arc::new(Tokens::load(db, config.remote_limits)?),
            status: Mutex::new(Status::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The tokens.
    pub fn tokens(&self) -> &Arc<Tokens> {
        &self.tokens
    }

    /// The bound address, once listening.
    pub fn bound_addr(&self) -> Option<SocketAddr> {
        self.lock().bound
    }

    /// Whether it is listening.
    pub fn is_bound(&self) -> bool {
        self.bound_addr().is_some()
    }

    /// As `GetDaemonStatus` reports it.
    pub fn status(&self, config: &Config) -> v1::RemoteListener {
        let status = self.lock();
        let (certificate, cert_error) = match &status.resolver {
            Some(resolver) => {
                let (info, error) = resolver.info();
                (Some(certificate_proto(&info)), error)
            }
            None => (None, None),
        };
        let state = match (self.enabled_by, status.bound) {
            (None, _) => v1::RemoteListenerState::Off,
            (Some(_), Some(_)) => v1::RemoteListenerState::Listening,
            (Some(_), None) => v1::RemoteListenerState::Failed,
        };
        let enabled_by = match self.enabled_by {
            None => v1::RemoteEnabledBy::Unspecified,
            Some(EnabledBy::Config) => v1::RemoteEnabledBy::Config,
            Some(EnabledBy::Flag) => v1::RemoteEnabledBy::Flag,
        };
        // Before the first bind attempt, an enabled listener is not FAILED.
        let state = if state == v1::RemoteListenerState::Failed && status.error.is_none() {
            v1::RemoteListenerState::Unspecified
        } else {
            state
        };
        v1::RemoteListener {
            state: state.into(),
            enabled_by: enabled_by.into(),
            address: status.bound.map_or_else(
                || config.file.remote.listen.clone(),
                |addr| addr.to_string(),
            ),
            names: self.names.clone(),
            certificate,
            error: status.error.clone().or(cert_error).unwrap_or_default(),
            live_token_count: u32::try_from(self.tokens.live_count()).unwrap_or(u32::MAX),
        }
    }

    /// Step 11: starts the tokens' upkeep and, if enabled, the listener,
    /// which binds in the background and retries until it can. Both stop
    /// with the daemon, joined by a shutdown hook.
    pub fn start(state: &Arc<AppState>) {
        let remote = &state.remote;
        let upkeep = remote.tokens.run(state.shutdown.clone());
        let server = remote
            .enabled_by
            .map(|_| tokio::spawn(serve(Arc::clone(state))));
        state.hooks.add("remote listener", move || {
            Box::pin(async move {
                if let Some(mut server) = server {
                    match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!(%error, "the remote listener panicked"),
                        Err(_) => server.abort(),
                    }
                }
                let _ = upkeep.await;
            })
        });
    }
}

fn certificate_proto(info: &CertInfo) -> v1::RemoteCertificate {
    v1::RemoteCertificate {
        source: match info.source {
            Source::Generated => v1::RemoteCertificateSource::Generated,
            Source::File => v1::RemoteCertificateSource::File,
        }
        .into(),
        fingerprint: info.fingerprint.clone(),
        not_after: Some(prost_types::Timestamp {
            seconds: info.not_after.0.div_euclid(1000),
            nanos: i32::try_from(info.not_after.0.rem_euclid(1000) * 1_000_000).unwrap_or(0),
        }),
        subject_names: info.subject_names.clone(),
    }
}

/// Sleeps `wait`, or less if the daemon stops; whether it stopped.
async fn wait_or_stop(wait: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        () = tokio::time::sleep(wait) => false,
        () = shutdown.cancelled() => true,
    }
}

/// The generated certificate's names: the listener's, and the bound IP
/// when it is not a wildcard.
fn cert_names(state: &AppState) -> Vec<String> {
    let mut names = state.remote.names.clone();
    if let Some(addr) = state.config.file.remote.listen_addr()
        && !addr.ip().is_unspecified()
        && !names.contains(&addr.ip().to_string())
    {
        names.push(addr.ip().to_string());
    }
    names
}

/// The certificate `[remote]` asks for.
fn resolver(state: &AppState) -> Result<Resolver, super::tls::TlsError> {
    let remote = &state.config.file.remote;
    let limits = &state.config.remote_limits;
    if remote.tls_cert.is_empty() {
        Resolver::generated(&state.config.paths.remote, &cert_names(state), limits)
    } else {
        Resolver::supplied(
            std::path::Path::new(&remote.tls_cert),
            std::path::Path::new(&remote.tls_key),
            limits,
        )
    }
}

/// Loads the certificate and binds, retrying with backoff, then serves
/// until the daemon stops.
async fn serve(state: Arc<AppState>) {
    let limits: Limits = state.config.remote_limits;
    let shutdown = state.shutdown.clone();
    let listen = state.config.file.remote.listen.clone();
    let Some(addr) = state.config.file.remote.listen_addr() else {
        // Validated at load.
        return;
    };
    let mut wait = limits.bind_retry;
    let (listener, resolver) = loop {
        let attempt = match resolver(&state) {
            Ok(resolver) => match TcpListener::bind(addr).await {
                // Never SO_REUSEPORT, as for the web listener.
                Ok(listener) => Ok((listener, Arc::new(resolver))),
                Err(error) => Err(format!("cannot listen on {listen}: {error}")),
            },
            Err(error) => Err(format!("certificate: {}", ErrorChain(&error))),
        };
        match attempt {
            Ok(bound) => break bound,
            Err(error) => {
                tracing::warn!(address = %listen, %error, retry_ms = wait.as_millis(), "the remote listener cannot start; retrying");
                state.remote.lock().error = Some(error);
                if wait_or_stop(wait, &shutdown).await {
                    return;
                }
                wait = (wait * 2).min(limits.bind_retry_max);
            }
        }
    };
    let config = match super::tls::server_config(Arc::clone(&resolver)) {
        Ok(config) => config,
        Err(error) => {
            state.remote.lock().error = Some(format!("TLS: {error}"));
            tracing::error!(%error, "cannot configure the remote listener's TLS");
            return;
        }
    };
    let local = listener.local_addr().ok();
    let gate = Arc::new(Gate {
        names: state.remote.names.clone(),
        tokens: Arc::clone(&state.remote.tokens),
        failures: Arc::new(Failures::new(&limits)),
    });
    let incoming = super::incoming::incoming(
        listener,
        TlsAcceptor::from(Arc::new(config)),
        Arc::clone(&gate.failures),
        &limits,
        shutdown.clone(),
    );
    {
        let mut status = state.remote.lock();
        status.bound = local;
        status.error = None;
        status.resolver = Some(Arc::clone(&resolver));
    }
    let (info, _) = resolver.info();
    tracing::info!(
        address = %local.map_or(listen, |addr| addr.to_string()),
        fingerprint = %info.fingerprint,
        "remote listener serving"
    );
    let renewals = {
        let (state, resolver, shutdown) =
            (Arc::clone(&state), Arc::clone(&resolver), shutdown.clone());
        tokio::spawn(async move {
            while !wait_or_stop(limits.cert_check_interval, &shutdown).await {
                resolver.renew_if_due(&state.config.paths.remote, &cert_names(&state), &limits);
            }
        })
    };
    let served = crate::server::remote_router(&state, gate)
        .serve_with_incoming_shutdown(incoming, shutdown.clone().cancelled_owned())
        .await;
    if let Err(error) = served {
        tracing::warn!(error = %ErrorChain(&error), "the remote listener failed");
    }
    state.remote.lock().bound = None;
    renewals.abort();
}
