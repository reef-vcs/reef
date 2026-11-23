//! A web origin a pinned passkey's touch must run at (#346, ADR 0094).

use std::fmt;

/// The host the local web UI's pages are under, and its passkeys' RP ID. A
/// daemon serves at `<16 hex>.reef.localhost`, a name of its run.
pub const LOCAL_HOST: &str = "reef.localhost";

/// A web origin as a pin names it: `http://reef.localhost[:port]` for the
/// local web UI, or `https://<dns name>[:port]`. A port is written only when
/// it is not the scheme's own.
///
/// [`crate::Policy::presence`] compares it with a certificate's origin
/// ([`Origin::covers`]), so the grammar catches typing mistakes and decides
/// nothing: an origin no browser writes covers no touch.
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
        if http && host != LOCAL_HOST {
            return Err(
                "over http only Reef's own local pages are pinned, as http://reef.localhost:<port>, without the name of one run; another host needs https",
            );
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

    /// Whether a touch at `touched`, as a browser wrote it, ran at this
    /// origin: `touched` itself, byte for byte, or for the local web UI, a
    /// run's own name under it at the same port.
    pub fn covers(&self, touched: &str) -> bool {
        let Some(local) = self.0.strip_prefix("http://") else {
            return touched == self.0;
        };
        touched
            .strip_prefix("http://")
            .and_then(|rest| rest.strip_suffix(local))
            .and_then(|run| run.strip_suffix('.'))
            .is_some_and(is_run_name)
    }

    /// The host and, when it is not the scheme's own, the port: what a
    /// request's `Host` holds at this origin.
    pub fn authority(&self) -> &str {
        self.0.split_once("://").map_or("", |(_, rest)| rest)
    }

    /// The DNS name.
    pub fn host(&self) -> &str {
        let authority = self.authority();
        authority
            .split_once(':')
            .map_or(authority, |(host, _)| host)
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

/// Whether `label` is the name a daemon's run takes: 16 lowercase hex.
fn is_run_name(label: &str) -> bool {
    label.len() == 16
        && label
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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
    fn origins_a_pin_names() {
        for text in [
            "http://reef.localhost:17333",
            "http://reef.localhost",
            "https://reef.example",
            "https://reef.example:8443",
            "https://a-b.1c.example",
        ] {
            assert_eq!(Origin::parse(text).map(|o| o.to_string()).unwrap(), text);
        }
        let origin = Origin::parse("https://reef.example:8443").unwrap();
        assert_eq!(origin.authority(), "reef.example:8443");
        assert_eq!(origin.host(), "reef.example");
        let origin = Origin::parse("https://reef.example").unwrap();
        assert_eq!(origin.authority(), "reef.example");
        assert_eq!(origin.host(), "reef.example");
    }

    #[test]
    fn origins_no_pin_names() {
        for (text, why) in [
            ("reef.localhost:17333", "does not start with"),
            ("HTTP://reef.localhost:17333", "does not start with"),
            ("http://reef.localhost:17333/", "no path"),
            ("http://reef.localhost:17333/passkeys", "no path"),
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
            ("http://localhost:17333", "over http only"),
            (
                "http://0123456789abcdef.reef.localhost:17333",
                "over http only",
            ),
            ("http://reef.example", "over http only"),
            ("http://reef.localhost:", "its port"),
            ("http://reef.localhost:0", "its port"),
            ("http://reef.localhost:017333", "its port"),
            ("http://reef.localhost:+17333", "its port"),
            ("http://reef.localhost:65536", "its port"),
            ("http://reef.localhost:1:2", "its port"),
            ("http://reef.localhost:80", "scheme's own port"),
            ("https://reef.example:443", "scheme's own port"),
        ] {
            let error = Origin::parse(text).unwrap_err();
            assert!(error.contains(why), "{text}: {error}");
        }
    }

    #[test]
    fn a_local_pin_covers_each_runs_name_at_its_port() {
        let local = Origin::parse("http://reef.localhost:17333").unwrap();
        for run in ["0123456789abcdef", "ffffffffffffffff"] {
            assert!(local.covers(&format!("http://{run}.reef.localhost:17333")));
        }
        for touched in [
            "http://0123456789abcdef.reef.localhost:17334",
            "http://0123456789abcdef.reef.localhost",
            "http://0123456789abcdef.reef.localhost:17333/",
            "https://0123456789abcdef.reef.localhost:17333",
            "http://reef.localhost:17333",
            "http://.reef.localhost:17333",
            "http://localhost:17333",
            "http://0123456789abcde.reef.localhost:17333",
            "http://0123456789ABCDEF.reef.localhost:17333",
            "http://evil.reef.localhost:17333",
            "http://evil.0123456789abcdef.reef.localhost:17333",
            "http://0123456789abcdef.evilreef.localhost:17333",
            "http://0123456789abcdefreef.localhost:17333",
            "http://0123456789abcdef.reef.localhost:117333",
            "",
        ] {
            assert!(!local.covers(touched), "{touched}");
        }
        let bare = Origin::parse("http://reef.localhost").unwrap();
        assert!(bare.covers("http://0123456789abcdef.reef.localhost"));
        assert!(!bare.covers("http://0123456789abcdef.reef.localhost:17333"));

        // Any other origin is one address.
        let https = Origin::parse("https://reef.example:8443").unwrap();
        assert!(https.covers("https://reef.example:8443"));
        for touched in [
            "https://0123456789abcdef.reef.example:8443",
            "https://reef.example",
            "http://reef.example:8443",
        ] {
            assert!(!https.covers(touched), "{touched}");
        }
    }
}
