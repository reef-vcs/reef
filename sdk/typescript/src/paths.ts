// Where the daemon listens, settled exactly as reef-client's paths.rs settles it.
//
// Precedence, lowest first: the platform default, config.toml's `[daemon] socket`, the
// environment (REEF_STATE_DIR, REEF_SOCKET, REEF_CONFIG), then an argument. Empty variables are
// unset; relative paths are made absolute against the current directory. The $XDG_RUNTIME_DIR
// socket is the default on Linux only for the default state directory. A socket path over 103
// bytes is an error. sdk/fixtures/socket-paths.json holds the cases this, the Python and the Rust
// implementations all pass.

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { isAbsolute } from "node:path";

import { parse as parseToml } from "smol-toml";

import { ConfigError } from "./errors.js";

export const STATE_DIR_ENV = "REEF_STATE_DIR";
export const SOCKET_ENV = "REEF_SOCKET";
export const CONFIG_ENV = "REEF_CONFIG";
export const SOCKET_PATH_MAX = 103;

/** Reads an environment variable. */
export type Env = (name: string) => string | undefined;

/** The platform's default state directory and, on Linux, its runtime directory. */
export interface PlatformDefaults {
  stateDir: string;
  runtimeDir?: string | undefined;
}

/** The paths a client needs. */
export interface Settled {
  stateDir: string;
  configFile: string;
  socket: string;
}

/**
 * Paths joined as Python's `os.path.join` and Rust's `Path::join` join them on Unix: an absolute
 * part replaces what came before, and nothing is normalized (`..` stays), unlike `node:path`'s.
 */
export function join(...parts: string[]): string {
  let out = "";
  for (const part of parts) {
    if (part.startsWith("/") || out === "") {
      out = part;
    } else {
      out = out.endsWith("/") ? out + part : `${out}/${part}`;
    }
  }
  return out;
}

/** The process environment, as an Env. */
export const processEnv: Env = (name) => process.env[name];

function absoluteXdg(value: string | undefined): string | undefined {
  // etcetera, which the daemon uses, ignores a relative or empty XDG variable.
  return value && isAbsolute(value) ? value : undefined;
}

/** This platform's defaults, from the home directory and the XDG variables. */
export function platformDefaults(
  env: Env,
  home?: string,
  platform: string = process.platform,
): PlatformDefaults {
  let found = home;
  if (found === undefined) {
    found = env("HOME") || undefined;
    if (found === undefined) {
      try {
        found = homedir() || undefined;
      } catch {
        found = undefined;
      }
    }
  }
  if (!found) {
    throw new ConfigError(
      `cannot find the home directory for the default state directory; set ${STATE_DIR_ENV}`,
    );
  }
  if (platform === "darwin") {
    return { stateDir: join(found, "Library", "Application Support", "reef") };
  }
  const state = absoluteXdg(env("XDG_STATE_HOME")) ?? join(found, ".local", "state");
  return { stateDir: join(state, "reef"), runtimeDir: absoluteXdg(env("XDG_RUNTIME_DIR")) };
}

/**
 * `path` made absolute as Rust's `std::path::absolute` makes it on Unix: relative paths are
 * joined to the current directory; repeated separators and `.` components go, while `..` and a
 * trailing separator stay. Nothing on disk is read.
 */
export function absolute(path: string, cwd?: string): string {
  if (!path) {
    throw new ConfigError("resolve : the path is empty", { kind: "io" });
  }
  let full = path;
  if (!full.startsWith("/")) {
    const base = cwd ?? process.cwd();
    full = base.replace(/\/+$/, "") + "/" + full;
  }
  const lead = full.startsWith("//") && !full.startsWith("///") ? "//" : "/";
  const parts = full.split("/").filter((part) => part !== "" && part !== ".");
  const trailing = full.endsWith("/") && parts.length > 0 ? "/" : "";
  return lead + parts.join("/") + trailing;
}

/** The state directory alone: the argument, else REEF_STATE_DIR, else the default. */
export function settleStateDir(
  options: { stateDir?: string | undefined; env?: Env; cwd?: string } = {},
): string {
  const env = options.env ?? processEnv;
  if (options.stateDir !== undefined) {
    return absolute(options.stateDir, options.cwd);
  }
  const fromEnv = env(STATE_DIR_ENV);
  if (fromEnv) {
    return absolute(fromEnv, options.cwd);
  }
  return platformDefaults(env).stateDir;
}

/** What settles the paths: arguments, the environment, and the platform's defaults. */
export interface SettleOptions {
  socket?: string | undefined;
  stateDir?: string | undefined;
  config?: string | undefined;
  env?: Env | Readonly<Record<string, string>> | undefined;
  defaults?: (() => PlatformDefaults) | undefined;
  cwd?: string | undefined;
}

/** The state directory, config file and socket, as the CLI and the daemon settle them. */
export function settle(options: SettleOptions = {}): Settled {
  const given = options.env;
  const env: Env =
    given === undefined
      ? processEnv
      : typeof given === "function"
        ? given
        : (name) => (Object.hasOwn(given, name) ? given[name] : undefined);
  const pick = (
    value: string | undefined,
    variable: string,
  ): { path: string; source: "argument" | "env" } | undefined => {
    if (value !== undefined) {
      return { path: absolute(value, options.cwd), source: "argument" };
    }
    const fromEnv = env(variable);
    if (fromEnv) {
      return { path: absolute(fromEnv, options.cwd), source: "env" };
    }
    return undefined;
  };

  const state = pick(options.stateDir, STATE_DIR_ENV);
  const sock = pick(options.socket, SOCKET_ENV);
  const conf = pick(options.config, CONFIG_ENV);

  let stateDir: string;
  let runtimeDir: string | undefined;
  if (state !== undefined) {
    stateDir = state.path;
  } else {
    const platform = (options.defaults ?? (() => platformDefaults(env)))();
    stateDir = platform.stateDir;
    runtimeDir = platform.runtimeDir;
  }
  const configFile = conf?.path ?? join(stateDir, "config.toml");

  const fileSocket = readSocketKey(configFile, conf?.source);
  let socket: string;
  if (sock !== undefined) {
    socket = sock.path;
  } else if (fileSocket !== undefined) {
    socket = join(stateDir, fileSocket);
  } else if (runtimeDir !== undefined) {
    socket = join(runtimeDir, "reef", "daemon.sock");
  } else {
    socket = join(stateDir, "daemon.sock");
  }
  const length = Buffer.byteLength(socket);
  if (length > SOCKET_PATH_MAX) {
    throw new ConfigError(
      `socket path ${socket} is ${length} bytes; the limit is ${SOCKET_PATH_MAX}. ` +
        `Set ${SOCKET_ENV} or use a shorter state directory`,
    );
  }
  return { stateDir, configFile, socket };
}

function errnoOf(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String(error.code)
    : undefined;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Reads a TOML file, mapping failures to ConfigError; undefined when it is absent. */
export function readToml(
  path: string,
  describe: string,
  absent: () => Record<string, unknown> | undefined,
): Record<string, unknown> | undefined {
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch (error) {
    const code = errnoOf(error);
    if (code === "ENOENT") {
      return absent();
    }
    if (code === "EACCES" || code === "EPERM") {
      throw new ConfigError(`read ${describe} ${path}: permission denied`, {
        kind: "permission_denied",
        cause: error,
      });
    }
    throw new ConfigError(`read ${describe} ${path}: ${messageOf(error)}`, {
      kind: "io",
      cause: error,
    });
  }
  try {
    return parseToml(text);
  } catch (error) {
    throw new ConfigError(`parse ${describe} ${path}: ${messageOf(error)}`, { cause: error });
  }
}

function table(data: Record<string, unknown>, key: string, path: string): Record<string, unknown> {
  const value = data[key] ?? {};
  if (typeof value !== "object" || Array.isArray(value) || value instanceof Date) {
    throw new ConfigError(`parse the configuration file ${path}: [${key}] is not a table`);
  }
  return value as Record<string, unknown>;
}

/** `[daemon] socket` from the config file; the rest of the file is the daemon's. */
function readSocketKey(path: string, source: "argument" | "env" | undefined): string | undefined {
  const data = readToml(path, "the configuration file", () => {
    if (source !== undefined) {
      const named = source === "argument" ? "the config argument" : CONFIG_ENV;
      throw new ConfigError(`the configuration file ${path} does not exist (named by ${named})`, {
        kind: "not_found",
      });
    }
    return undefined;
  });
  if (data === undefined) {
    return undefined;
  }
  // The three keys a client reads, typed as the Rust client types them; the rest is ignored.
  const daemon = table(data, "daemon", path);
  const repo = table(data, "repo", path);
  const typed: [Record<string, unknown>, string, string][] = [
    [daemon, "pid_file", "string"],
    [repo, "auto_register", "boolean"],
  ];
  for (const [section, key, kind] of typed) {
    if (key in section && typeof section[key] !== kind) {
      throw new ConfigError(`parse the configuration file ${path}: ${key} has the wrong type`);
    }
  }
  const value = daemon["socket"];
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "string") {
    throw new ConfigError(`parse the configuration file ${path}: daemon.socket is not a string`);
  }
  return value;
}
