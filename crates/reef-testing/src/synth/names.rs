//! The embedded word list and the extension weights that file and directory
//! names, and file contents, are drawn from.
//!
//! Words are lowercase ASCII letters and digits only: no underscore, so the
//! `_<n>` collision suffix can never produce another word, and no capitals,
//! so no two names differ only in case on a case-insensitive file system.
//! None of them is an ignored directory name (`target`, `node_modules`).
//! Changing either list changes the output and needs a `GENERATOR_VERSION`
//! bump.

use super::dist::{SynthRng, index};

/// Plausible source-tree identifiers.
pub(crate) const WORDS: &[&str] = &[
    "access",
    "account",
    "action",
    "adapter",
    "admin",
    "agent",
    "alert",
    "alloc",
    "api",
    "app",
    "archive",
    "args",
    "asset",
    "async",
    "audit",
    "auth",
    "backend",
    "backup",
    "base",
    "batch",
    "bench",
    "billing",
    "bin",
    "binding",
    "blob",
    "block",
    "bridge",
    "buffer",
    "build",
    "bundle",
    "bus",
    "cache",
    "channel",
    "check",
    "chunk",
    "client",
    "cluster",
    "codec",
    "collector",
    "command",
    "common",
    "compat",
    "component",
    "compress",
    "config",
    "conn",
    "console",
    "context",
    "control",
    "core",
    "cron",
    "crypto",
    "daemon",
    "dashboard",
    "data",
    "db",
    "debug",
    "decode",
    "deploy",
    "detail",
    "device",
    "diff",
    "digest",
    "dispatch",
    "docs",
    "domain",
    "driver",
    "editor",
    "email",
    "encode",
    "engine",
    "entity",
    "env",
    "error",
    "event",
    "export",
    "extract",
    "feature",
    "fetch",
    "field",
    "filter",
    "fixture",
    "flag",
    "format",
    "frame",
    "gateway",
    "graph",
    "grpc",
    "guard",
    "handler",
    "handlers",
    "hash",
    "header",
    "health",
    "helper",
    "hook",
    "http",
    "image",
    "import",
    "index",
    "infra",
    "init",
    "input",
    "internal",
    "io",
    "item",
    "job",
    "kernel",
    "key",
    "layout",
    "lexer",
    "lib",
    "limit",
    "link",
    "list",
    "loader",
    "local",
    "lock",
    "log",
    "logger",
    "loop",
    "macro",
    "mailer",
    "main",
    "manager",
    "map",
    "media",
    "memory",
    "merge",
    "message",
    "meta",
    "metrics",
    "middleware",
    "migrations",
    "mock",
    "model",
    "monitor",
    "mount",
    "net",
    "node",
    "notify",
    "object",
    "order",
    "output",
    "packet",
    "page",
    "panel",
    "parser",
    "patch",
    "path",
    "payment",
    "peer",
    "pipeline",
    "plugin",
    "policy",
    "pool",
    "port",
    "process",
    "profile",
    "proto",
    "provider",
    "proxy",
    "query",
    "queue",
    "reader",
    "record",
    "redis",
    "registry",
    "render",
    "replica",
    "report",
    "request",
    "resolver",
    "response",
    "retry",
    "route",
    "router",
    "rpc",
    "runtime",
    "sample",
    "scanner",
    "schema",
    "scope",
    "search",
    "secret",
    "server",
    "service",
    "session",
    "shard",
    "shared",
    "signal",
    "snapshot",
    "socket",
    "source",
    "spec",
    "state",
    "storage",
    "store",
    "stream",
    "sync",
    "task",
    "template",
    "tenant",
    "test",
    "theme",
    "thread",
    "token",
    "tools",
    "trace",
    "transport",
    "tree",
    "types",
    "ui",
    "upload",
    "user",
    "util",
    "utils",
    "v1",
    "v2",
    "validate",
    "value",
    "vendor",
    "view",
    "watch",
    "web",
    "worker",
    "writer",
    "zone",
];

/// A text file extension and its weight out of [`TEXT_WEIGHT_TOTAL`].
const TEXT_EXTENSIONS: &[(&str, u64)] = &[
    ("rs", 20),
    ("py", 18),
    ("ts", 18),
    ("go", 14),
    ("md", 8),
    ("json", 7),
    ("toml", 4),
    ("yaml", 6),
    ("txt", 5),
];
const TEXT_WEIGHT_TOTAL: u64 = 100;

/// Extensions for the large incompressible files.
const BINARY_EXTENSIONS: &[&str] = &["bin", "png", "wasm"];

/// A random word from [`WORDS`].
pub(crate) fn word(rng: &mut SynthRng) -> &'static str {
    WORDS[index(rng, WORDS.len())]
}

/// A directory name before collision handling: one word.
pub(crate) fn dir_name(rng: &mut SynthRng) -> &'static str {
    word(rng)
}

/// A text file's stem and extension before collision handling.
pub(crate) fn file_name(rng: &mut SynthRng) -> (&'static str, &'static str) {
    let stem = word(rng);
    let mut pick = super::dist::below(rng, TEXT_WEIGHT_TOTAL);
    for &(ext, weight) in TEXT_EXTENSIONS {
        if pick < weight {
            return (stem, ext);
        }
        pick -= weight;
    }
    unreachable!("extension weights sum to TEXT_WEIGHT_TOTAL")
}

/// A binary file's stem and extension before collision handling.
pub(crate) fn binary_name(rng: &mut SynthRng) -> (&'static str, &'static str) {
    let stem = word(rng);
    (stem, BINARY_EXTENSIONS[index(rng, BINARY_EXTENSIONS.len())])
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn words_are_safe_names() {
        let mut seen = HashSet::new();
        for word in WORDS {
            assert!(
                word.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "{word}"
            );
            assert!(!["target", "node_modules"].contains(word), "{word}");
            assert!(seen.insert(word), "duplicate {word}");
        }
        assert!(WORDS.len() >= 200, "{}", WORDS.len());
    }

    #[test]
    fn weights_sum_to_total() {
        let total: u64 = TEXT_EXTENSIONS.iter().map(|&(_, w)| w).sum();
        assert_eq!(total, TEXT_WEIGHT_TOTAL);
    }
}
