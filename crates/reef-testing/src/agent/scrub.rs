//! Removing credentials from what a benchmark saves: streams, logs, trial
//! files, summaries.

/// Variables whose values are credentials, scrubbed wherever they appear.
pub const SECRET_VARIABLES: [&str; 1] = ["ANTHROPIC_API_KEY"];

/// Removes credentials from a saved stream or log: the value of every
/// [`SECRET_VARIABLES`] entry wherever it appears, anything that looks like
/// an Anthropic key, and the value of any `Authorization` header.
pub fn scrub(text: &str) -> String {
    let secrets: Vec<String> = SECRET_VARIABLES
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .collect();
    scrub_values(text, &secrets)
}

/// [`scrub`] with the secrets' values given: each one of eight characters
/// or more is replaced wherever it appears.
fn scrub_values(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    for secret in secrets.iter().filter(|s| s.len() >= 8) {
        text = text.replace(secret.as_str(), "[scrubbed]");
    }
    text = scrub_after(&text, "sk-ant-", |c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '_'
    });
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&scrub_authorization(line));
    }
    out
}

/// Replaces the run of `keep` characters after each `prefix` with
/// `[scrubbed]`.
fn scrub_after(text: &str, prefix: &str, keep: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        out.push_str(&rest[..at]);
        out.push_str("[scrubbed]");
        let after = &rest[at + prefix.len()..];
        let end = after.find(|c: char| !keep(c)).unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// `Authorization: Bearer x` and `"authorization":"Bearer x"` with the value
/// replaced, case-insensitively.
fn scrub_authorization(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let Some(at) = lower.find("authorization") else {
        return line.to_owned();
    };
    let after = at + "authorization".len();
    let rest = &line[after..];
    let Some(colon) = rest.find(':') else {
        return line.to_owned();
    };
    if !rest[..colon]
        .chars()
        .all(|c| c == '"' || c == '\\' || c.is_whitespace())
    {
        return line.to_owned();
    }
    let value = &rest[colon + 1..];
    let start = value.len() - value.trim_start_matches([' ', '"', '\\']).len();
    let end = value[start..]
        .find(['"', '\\', '\n', ','])
        .map_or(value.len(), |e| start + e);
    format!(
        "{}{}[scrubbed]{}",
        &line[..=after + colon],
        &value[..start],
        scrub_authorization(&value[end..])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 2: the key's value wherever it appears, anything that looks
    /// like a key, and `Authorization` values are scrubbed.
    #[test]
    fn scrubs_keys_and_authorization() {
        assert_eq!(
            scrub("key sk-ant-api03-AbC_d-9 end\n"),
            "key [scrubbed] end\n"
        );
        assert_eq!(
            scrub("Authorization: Bearer abc.def\n"),
            "Authorization: [scrubbed]\n"
        );
        assert_eq!(
            scrub("{\"headers\":{\"authorization\":\"Bearer x\",\"a\":1}}"),
            "{\"headers\":{\"authorization\":\"[scrubbed]\",\"a\":1}}"
        );
        assert_eq!(
            scrub(r#"{"content":"curl -H \"Authorization: Bearer x\" u"}"#),
            r#"{"content":"curl -H \"Authorization: [scrubbed]\" u"}"#
        );
        assert_eq!(
            scrub("x-api-key: sk-ant-api03-zz\n"),
            "x-api-key: [scrubbed]\n"
        );
        // A secret's value, in whatever shape; too short a value is left.
        let secrets = ["plain-test-value-1234".to_owned(), "short".to_owned()];
        assert_eq!(
            scrub_values("v=plain-test-value-1234; short", &secrets),
            "v=[scrubbed]; short"
        );
        let clean = "{\"type\":\"result\",\"result\":\"authorization flow\"}\n";
        assert_eq!(scrub(clean), clean);
    }
}
