// The options the client's calls and `connect()` take.

/** Options for one call. */
export interface CallOptions {
  /** Ends the call, or the stream, when aborted. */
  signal?: AbortSignal | undefined;
  /**
   * The call's deadline in milliseconds; for a page iterator, each page's. Unary calls only:
   * streams end by `signal`, `cancel()` or leaving `await using`. Without it, the client's
   * `timeoutMs`, if any.
   */
  timeoutMs?: number | undefined;
}

/** What `connect()` reaches, and how. */
export interface ConnectOptions {
  /** The daemon's socket; else `REEF_SOCKET`, else `config.toml`'s, else the default. */
  socket?: string | undefined;
  /**
   * A remote daemon: an `https://` URL, or a profile's name in `<state>/remotes.toml`. Else
   * `REEF_REMOTE`. Not with `socket`.
   */
  remote?: string | undefined;
  /** The remote's bearer token; else `REEF_REMOTE_TOKEN`, else the profile's. */
  token?: string | undefined;
  /** The remote's key to pin, `sha256:<hex>`; else `REEF_REMOTE_FINGERPRINT`, else the profile's. */
  fingerprint?: string | undefined;
  /** PEM certificates to trust for a remote without a pin, instead of Node's and the OS's roots. */
  rootCertificates?: string[] | undefined;
  /** The state directory; else `REEF_STATE_DIR`, else the platform's. */
  stateDir?: string | undefined;
  /** The configuration file; else `REEF_CONFIG`, else `<state>/config.toml`. */
  config?: string | undefined;
  /** The agent named on every call (`reef-agent`); else `REEF_AGENT`. */
  agent?: string | undefined;
  /** Every unary call's default deadline in milliseconds; none by default. */
  timeoutMs?: number | undefined;
  /** How long connecting may take, in milliseconds; 5,000 by default. */
  connectTimeoutMs?: number | undefined;
}
