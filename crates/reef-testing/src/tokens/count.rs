//! Counting text: bytes, Anthropic's token counting endpoint, and the
//! published `o200k_base` encoding. Three counters, because the ratio between two
//! outputs depends on the tokenizer (JSON with hex ids tokenizes worse than
//! prose), and one vendor's tokenizer alone would make that vendor's model
//! the benchmark's reference.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// A way of counting text.
pub trait Counter {
    /// Its name in results and tables: `bytes`, `anthropic`, `o200k`.
    fn name(&self) -> &'static str;
    /// The count of `text`.
    fn count(&mut self, text: &str) -> Result<u64, String>;
    /// The count of tool definitions (name, description, input schema
    /// each); by default, of their compact JSON.
    fn count_tools(&mut self, definitions: &[Value]) -> Result<u64, String> {
        self.count(&Value::Array(definitions.to_vec()).to_string())
    }
}

/// UTF-8 bytes: vendor-neutral and free.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bytes;

impl Counter for Bytes {
    fn name(&self) -> &'static str {
        "bytes"
    }

    fn count(&mut self, text: &str) -> Result<u64, String> {
        Ok(text.len() as u64)
    }
}

/// The published `o200k_base` encoding (`OpenAI`'s), offline, through `tiktoken-rs`: a
/// published encoding from a second vendor, deterministic and free.
#[cfg(feature = "tokenizers")]
pub struct O200k(tiktoken_rs::CoreBPE);

#[cfg(feature = "tokenizers")]
impl O200k {
    /// Loads the encoding (bundled with the crate).
    pub fn new() -> Result<Self, String> {
        tiktoken_rs::o200k_base()
            .map(O200k)
            .map_err(|e| format!("o200k_base: {e}"))
    }
}

#[cfg(feature = "tokenizers")]
impl Counter for O200k {
    fn name(&self) -> &'static str {
        "o200k"
    }

    fn count(&mut self, text: &str) -> Result<u64, String> {
        Ok(self.0.encode_ordinary(text).len() as u64)
    }
}

/// The `tiktoken-rs` version rows record, or why there is none.
pub const O200K_SOURCE: &str = if cfg!(feature = "tokenizers") {
    "tiktoken-rs 0.12.1"
} else {
    "not built (feature `tokenizers`)"
};

/// Anthropic's `POST /v1/messages/count_tokens` for one model: each text
/// sent as one user message, less the count of a one-character message.
/// Requests go one at a time through `curl`, the key passed in a config on
/// stdin so it never appears in a process's arguments; a 429 is retried
/// after its `retry-after`. Counts are cached by digest in a JSON file.
pub struct Anthropic {
    model: String,
    key: String,
    curl: PathBuf,
    url: String,
    cache_file: Option<PathBuf>,
    cache: BTreeMap<String, u64>,
    baseline: Option<u64>,
}

/// The endpoint.
pub const COUNT_TOKENS_URL: &str = "https://api.anthropic.com/v1/messages/count_tokens";

impl Anthropic {
    /// A counter for `model`, with the key from `ANTHROPIC_API_KEY`; `None`
    /// without one. The cache lives in `cache_file` if given.
    pub fn from_env(model: &str, cache_file: Option<&Path>) -> Option<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())?;
        Some(Anthropic::new(
            model,
            key,
            PathBuf::from("curl"),
            COUNT_TOKENS_URL,
            cache_file,
        ))
    }

    /// A counter with every part named, for tests.
    pub fn new(
        model: &str,
        key: String,
        curl: PathBuf,
        url: &str,
        cache_file: Option<&Path>,
    ) -> Self {
        let cache = cache_file
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Anthropic {
            model: model.to_owned(),
            key,
            curl,
            url: url.to_owned(),
            cache_file: cache_file.map(Path::to_path_buf),
            cache,
            baseline: None,
        }
    }

    /// The model counted for.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// What `tools` (name, description, input schema each) add to a
    /// request's input, as the model sees them: the count with the tools
    /// less the count with one trivial tool. Not less the count with none:
    /// any tool brings the API's tool-use preamble, which a shell arm pays
    /// for its own tools too.
    pub fn tools_cost(&mut self, tools: &[Value]) -> Result<u64, String> {
        let with = self.request(&json!({
            "model": self.model,
            "messages": [{"role": "user", "content": "x"}],
            "tools": tools,
        }))?;
        let trivial = self.request(&json!({
            "model": self.model,
            "messages": [{"role": "user", "content": "x"}],
            "tools": [{"name": "t", "description": "t", "input_schema": {"type": "object"}}],
        }))?;
        Ok(with.saturating_sub(trivial))
    }

    fn baseline(&mut self) -> Result<u64, String> {
        if let Some(baseline) = self.baseline {
            return Ok(baseline);
        }
        let baseline = self.request(&json!({
            "model": self.model,
            "messages": [{"role": "user", "content": "x"}],
        }))?;
        self.baseline = Some(baseline);
        Ok(baseline)
    }

    /// `input_tokens` for one request body, from the cache or the endpoint.
    fn request(&mut self, body: &Value) -> Result<u64, String> {
        let body = body.to_string();
        let digest = hex(&Sha256::digest(body.as_bytes()));
        if let Some(count) = self.cache.get(&digest) {
            return Ok(*count);
        }
        let mut attempt = 0;
        let count = loop {
            attempt += 1;
            match self.post(&body)? {
                Reply::Count(count) => break count,
                Reply::Retry(after) if attempt < 6 => std::thread::sleep(after),
                Reply::Retry(_) => return Err("count_tokens: still rate limited".to_owned()),
            }
        };
        self.cache.insert(digest, count);
        if let Some(path) = &self.cache_file {
            let text = serde_json::to_string_pretty(&self.cache).map_err(|e| e.to_string())?;
            std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(count)
    }

    fn post(&self, body: &str) -> Result<Reply, String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let body_file = dir.path().join("body.json");
        std::fs::write(&body_file, body).map_err(|e| e.to_string())?;
        let mut child = Command::new(&self.curl)
            .args([
                "--silent",
                "--show-error",
                "--max-time",
                "60",
                "--config",
                "-",
            ])
            .args(["--header", "anthropic-version: 2023-06-01"])
            .args(["--header", "content-type: application/json"])
            .arg("--data-binary")
            .arg(format!("@{}", body_file.display()))
            .args(["--write-out", "\n%{http_code} %header{retry-after}"])
            .arg(&self.url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("curl: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("curl: no stdin")?
            .write_all(format!("header = \"x-api-key: {}\"\n", self.key).as_bytes())
            .map_err(|e| format!("curl: {e}"))?;
        let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "curl exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (json, status) = text.rsplit_once('\n').unwrap_or(("", &text));
        let mut status = status.split_whitespace();
        let code = status.next().unwrap_or("");
        match code {
            "200" => {
                let value: Value =
                    serde_json::from_str(json).map_err(|e| format!("count_tokens: {e}"))?;
                value["input_tokens"]
                    .as_u64()
                    .map(Reply::Count)
                    .ok_or_else(|| format!("count_tokens: no input_tokens in {json}"))
            }
            "429" | "529" => {
                let secs = status.next().and_then(|s| s.parse().ok()).unwrap_or(10);
                Ok(Reply::Retry(Duration::from_secs(secs)))
            }
            _ => Err(format!(
                "count_tokens answered {code}: {}",
                crate::agent::scrub::scrub(json.trim())
            )),
        }
    }
}

enum Reply {
    Count(u64),
    Retry(Duration),
}

impl Counter for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn count(&mut self, text: &str) -> Result<u64, String> {
        if text.is_empty() {
            return Ok(0);
        }
        let with = self.request(&json!({
            "model": self.model,
            "messages": [{"role": "user", "content": text}],
        }))?;
        let without = self.baseline()?;
        Ok(with.saturating_sub(without))
    }

    fn count_tools(&mut self, definitions: &[Value]) -> Result<u64, String> {
        self.tools_cost(definitions)
    }
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Counters that did not run, each with why.
pub type Missing = Vec<(&'static str, String)>;

/// The counters available: bytes always, `anthropic` with a key, `o200k`
/// with the `tokenizers` feature. Each one missing is named in the second
/// list with why.
pub fn available(model: &str, cache_file: Option<&Path>) -> (Vec<Box<dyn Counter>>, Missing) {
    let mut counters: Vec<Box<dyn Counter>> = vec![Box::new(Bytes)];
    let mut missing = Vec::new();
    match Anthropic::from_env(model, cache_file) {
        Some(counter) => counters.push(Box::new(counter)),
        None => missing.push(("anthropic", "ANTHROPIC_API_KEY is not set".to_owned())),
    }
    #[cfg(feature = "tokenizers")]
    match O200k::new() {
        Ok(counter) => counters.push(Box::new(counter)),
        Err(error) => missing.push(("o200k", error)),
    }
    #[cfg(not(feature = "tokenizers"))]
    missing.push(("o200k", "built without the `tokenizers` feature".to_owned()));
    (counters, missing)
}

/// Every counter's name, in table order.
pub const COUNTERS: [&str; 3] = ["bytes", "anthropic", "o200k"];

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// A fake `curl` that appends its arguments and stdin to a log and
    /// answers from a script: a 429 once, then counts by body length.
    fn fake_curl(dir: &Path) -> PathBuf {
        let curl = dir.join("curl");
        std::fs::write(
            &curl,
            format!(
                r#"#!/bin/sh
log={log}
echo "ARGS $*" >> "$log"
echo "STDIN $(cat)" >> "$log"
body=""
for a in "$@"; do case "$a" in @*) body="${{a#@}}";; esac; done
if [ ! -e {dir}/limited ]; then touch {dir}/limited; printf '{{"error":"rate"}}\n429 0'; exit 0; fi
n=$(wc -c < "$body" | tr -d ' ')
printf '{{"input_tokens":%s}}\n200 ' "$n"
"#,
                log = dir.join("log").display(),
                dir = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        curl
    }

    /// Test 6: bytes count bytes; the Anthropic counter keeps the key out
    /// of argv, retries a 429, subtracts the baseline, and caches by
    /// digest; `o200k` gives known counts.
    #[test]
    fn counters() {
        assert_eq!(Bytes.count("héllo").unwrap(), 6);

        let dir = tempfile::tempdir().unwrap();
        let curl = fake_curl(dir.path());
        let cache = dir.path().join("cache.json");
        let mut counter = Anthropic::new(
            "m",
            "sk-ant-test-key".to_owned(),
            curl.clone(),
            "http://localhost/count",
            Some(&cache),
        );
        let long = counter.count("hello world").unwrap();
        let short = counter.count("hi").unwrap();
        // The fake counts body bytes: the difference is the texts'.
        assert_eq!(long - short, "hello world".len() as u64 - "hi".len() as u64);
        let calls = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(
            calls
                .lines()
                .filter(|l| l.starts_with("ARGS"))
                .all(|l| !l.contains("sk-ant")),
            "{calls}"
        );
        assert!(
            calls.contains("STDIN header = \"x-api-key: sk-ant-test-key\""),
            "{calls}"
        );
        // One 429, then the text, the baseline and the second text.
        assert_eq!(calls.matches("ARGS").count(), 4, "{calls}");

        // A new counter reads the cache, the baseline's included: no
        // request.
        let mut again = Anthropic::new(
            "m",
            "k".to_owned(),
            curl,
            "http://localhost/count",
            Some(&cache),
        );
        assert_eq!(again.count("hello world").unwrap(), long);
        let calls = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert_eq!(calls.matches("ARGS").count(), 4, "{calls}");

        #[cfg(feature = "tokenizers")]
        {
            let mut o200k = O200k::new().unwrap();
            assert_eq!(o200k.count("hello world").unwrap(), 2);
            assert_eq!(o200k.count("").unwrap(), 0);
        }
    }
}
