// Choosing a remote daemon (R2.08, ADR 0049), as reef-client's remote.rs and the CLI do.
//
// REEF_REMOTE (or the `remote` option) is a profile's name in <state>/remotes.toml, or an
// https:// URL, which then needs a token. REEF_REMOTE_TOKEN and REEF_REMOTE_FINGERPRINT (or the
// options) override a profile's. sdk/fixtures/remote-urls.json holds the URL cases this, the
// Python and the Rust implementations all pass.

import { isIPv6 } from "node:net";

import { ConfigError } from "./errors.js";
import { type Env, join, processEnv, readToml } from "./paths.js";

export const REMOTE_ENV = "REEF_REMOTE";
export const TOKEN_ENV = "REEF_REMOTE_TOKEN";
export const FINGERPRINT_ENV = "REEF_REMOTE_FINGERPRINT";
export const PROFILES_FILE = "remotes.toml";

/** An `https://` URL's host and port. */
export class RemoteUrl {
  /** A DNS name, or an IP address without brackets. */
  readonly host: string;
  readonly port: number;

  constructor(host: string, port: number) {
    this.host = host;
    this.port = port;
  }

  /** `text` if it is `https://<host>[:<port>][/]`, else undefined. */
  static parse(text: string): RemoteUrl | undefined {
    if (!text.startsWith("https://")) {
      return undefined;
    }
    let rest = text.slice("https://".length);
    if (rest.endsWith("/")) {
      rest = rest.slice(0, -1);
    }
    if (!rest || /[/?#@]/.test(rest)) {
      return undefined;
    }
    let host: string;
    let port: number | undefined;
    if (rest.startsWith("[")) {
      const close = rest.indexOf("]");
      if (close < 0) {
        return undefined;
      }
      host = rest.slice(1, close);
      const after = rest.slice(close + 1);
      // Rust's Ipv6Addr takes no zone id.
      if (host.includes("%") || !isIPv6(host)) {
        return undefined;
      }
      if (after === "") {
        port = 443;
      } else if (after.startsWith(":")) {
        port = parsePort(after.slice(1));
      } else {
        return undefined;
      }
    } else {
      const colon = rest.lastIndexOf(":");
      if (colon >= 0) {
        host = rest.slice(0, colon);
        port = parsePort(rest.slice(colon + 1));
      } else {
        host = rest;
        port = 443;
      }
      if (host.includes(":")) {
        return undefined;
      }
    }
    if (port === undefined || !host || port === 0) {
      return undefined;
    }
    return new RemoteUrl(host, port);
  }

  /** `<host>:<port>`, bracketing an IPv6 host. */
  get authority(): string {
    return authority(this.host, this.port);
  }

  /** `https://<authority>`. */
  get origin(): string {
    return `https://${this.authority}`;
  }
}

/** `<host>:<port>`, bracketing an IPv6 host. */
export function authority(host: string, port: number): string {
  return host.includes(":") ? `[${host}]:${port}` : `${host}:${port}`;
}

function parsePort(text: string): number | undefined {
  // Rust's u16 parse: ASCII digits only (an optional `+`), at most 65535.
  const digits = text.startsWith("+") ? text.slice(1) : text;
  if (!/^[0-9]+$/.test(digits)) {
    return undefined;
  }
  const value = Number(digits);
  return value <= 65535 ? value : undefined;
}

/** Whether `text` is `sha256:` and 64 lowercase hex digits. */
export function isFingerprint(text: string): boolean {
  return /^sha256:[0-9a-f]{64}$/.test(text);
}

/** A remote daemon to connect to. */
export interface RemoteTarget {
  url: RemoteUrl;
  token: string;
  fingerprint: string | undefined;
  /** The profile's name; undefined for a URL. */
  name: string | undefined;
}

/** How errors name a remote: the profile's name, else the URL. */
export function labelOf(target: RemoteTarget): string {
  return target.name ?? target.url.origin;
}

interface Profile {
  url: string;
  token: string;
  fingerprint?: string;
}

/** The target `remote` names, with the token and pin from the options or the environment. */
export function choose(
  remote: string,
  options: {
    stateDir: string;
    token?: string | undefined;
    fingerprint?: string | undefined;
    env?: Env;
  },
): RemoteTarget {
  const env = options.env ?? processEnv;
  const fromEnv = (name: string): string | undefined => {
    const value = env(name);
    return value?.trim() ? value : undefined;
  };
  let token = options.token?.trim() || undefined;
  token ??= fromEnv(TOKEN_ENV)?.trim();
  const fingerprint = options.fingerprint || fromEnv(FINGERPRINT_ENV);
  if (remote.startsWith("https://")) {
    const url = RemoteUrl.parse(remote);
    if (url === undefined) {
      throw new ConfigError(`${JSON.stringify(remote)} is not https://<host>[:<port>]`);
    }
    if (token === undefined) {
      throw new ConfigError(`${REMOTE_ENV} names a URL, and ${TOKEN_ENV} is not set`, {
        hint: `Set ${TOKEN_ENV} to the token, or save a profile with \`reef remote add\`.`,
      });
    }
    return { url, token, fingerprint, name: undefined };
  }
  const profile = loadProfiles(options.stateDir).get(remote);
  if (profile === undefined) {
    throw new ConfigError(`no remote named ${remote}`, {
      kind: "not_found",
      hint: "`reef remote list` lists the saved ones; `reef remote add` saves one.",
      next: ["reef", "remote", "list"],
    });
  }
  const url = RemoteUrl.parse(profile.url);
  if (url === undefined) {
    throw new ConfigError(`the remote ${remote} has an invalid URL ${JSON.stringify(profile.url)}`);
  }
  return {
    url,
    token: token ?? profile.token.trim(),
    fingerprint: fingerprint ?? profile.fingerprint,
    name: remote,
  };
}

/** `<state>/remotes.toml`'s profiles; none when the file is absent. */
export function loadProfiles(stateDir: string): Map<string, Profile> {
  const path = join(stateDir, PROFILES_FILE);
  const data = readToml(path, "the profiles file", () => undefined);
  const profiles = new Map<string, Profile>();
  if (data === undefined) {
    return profiles;
  }
  for (const [name, value] of Object.entries(data)) {
    const profile = value as Record<string, unknown>;
    const valid =
      typeof value === "object" &&
      value !== null &&
      !Array.isArray(value) &&
      typeof profile["url"] === "string" &&
      typeof profile["token"] === "string" &&
      Object.keys(profile).every((key) => ["url", "token", "fingerprint"].includes(key));
    if (!valid) {
      throw new ConfigError(`${path} is not a valid profiles file: the profile ${name} is wrong`);
    }
    const pinned = profile["fingerprint"];
    profiles.set(name, {
      url: profile["url"] as string,
      token: profile["token"] as string,
      ...(typeof pinned === "string" ? { fingerprint: pinned } : {}),
    });
  }
  return profiles;
}
