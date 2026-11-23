//! A web origin a pinned passkey's touch must run at (#346, ADR 0094).

use std::fmt;

/// A web origin in the form a browser writes in `clientDataJSON.origin`:
/// `http://localhost[:port]`, or `https://<dns name>[:port]`, where its host
/// is the passkey's RP ID. A port is written only when it is not the
/// scheme's own.
///
/// [`crate::Policy::presence`] compares it with a certificate's origin byte
/// for byte, so the grammar catches typing mistakes and decides nothing: an
/// origin no browser writes matches no touch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin(String);

impl Origin {
    /// The origin `text` is.
    ///
    /// # Errors
    ///
    /// What about it is not an origin in that form.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let (http, authority) = if let Some(rest) = text.strip_prefix("http://") {
            (true, rest)
        } else if let Some(rest) = text.strip_prefix("https://") {
            (false, rest)
        } else {
            return Err("it does not start with http:// or https://");
        };
        if authority.contains(['/', '?', '#']) {
            return Err("an origin has no path and no trailing slash");
        }
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if !is_dns_name(host) {
            return Err(
                "its host is not a lowercase DNS name (browsers run no passkey ceremony at an IP address)",
            );
        }
        if http && host != "localhost" {
            return Err("http is for localhost only; another host needs https");
        }
        if let Some(port) = port {
            if port.starts_with('0')
                || !port.bytes().all(|b| b.is_ascii_digit())
                || port.parse::<u16>().is_err()
            {
                return Err("its port is not 1 to 65535");
            }
            if port == if http { "80" } else { "443" } {
                return Err("a browser leaves out the scheme's own port; leave it out here too");
            }
        }
        Ok(Self(text.to_owned()))
    }

    /// The origin's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `host` is dot-separated lowercase labels with a last one that is
/// not a number, so not an IPv4 address.
fn is_dns_name(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && host
            .rsplit('.')
            .next()
            .is_some_and(|last| !last.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_a_browser_writes() {
        for text in [
            "http://localhost:17333",
            "http://localhost",
            "https://localhost:17333",
            "https://reef.example",
            "https://reef.example:8443",
            "https://a-b.1c.example",
        ] {
            assert_eq!(Origin::parse(text).map(|o| o.to_string()).unwrap(), text);
        }
    }

    #[test]
    fn origins_no_browser_writes() {
        for (text, why) in [
            ("localhost:17333", "does not start with"),
            ("HTTP://localhost:17333", "does not start with"),
            ("http://localhost:17333/", "no path"),
            ("http://localhost:17333/passkeys", "no path"),
            ("https://reef.example?x", "no path"),
            ("http://127.0.0.1:17333", "DNS name"),
            ("https://127.0.0.1", "DNS name"),
            ("http://[::1]:17333", "DNS name"),
            ("https://Reef.example", "DNS name"),
            ("https://", "DNS name"),
            ("https://reef.example.", "DNS name"),
            ("https://-reef.example", "DNS name"),
            ("https://alice@reef.example", "DNS name"),
            ("https://*.example", "DNS name"),
            ("http://reef.example", "localhost only"),
            ("http://localhost:", "its port"),
            ("http://localhost:0", "its port"),
            ("http://localhost:017333", "its port"),
            ("http://localhost:+17333", "its port"),
            ("http://localhost:65536", "its port"),
            ("http://localhost:1:2", "its port"),
            ("http://localhost:80", "scheme's own port"),
            ("https://reef.example:443", "scheme's own port"),
        ] {
            let error = Origin::parse(text).unwrap_err();
            assert!(error.contains(why), "{text}: {error}");
        }
    }
}
