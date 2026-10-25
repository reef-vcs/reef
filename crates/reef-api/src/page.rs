//! Pagination (R2.01, ADR 0029): one token format for every paged RPC.
//!
//! A page token is base64url, without padding, of the request's filter
//! hash (8 bytes, big-endian) followed by the cursor, an opaque string the
//! service chooses (`Workspaces.List`'s is `<created_at>:<id>` of the last
//! row sent). The hash binds a token to the filters it was issued for: the
//! same token sent with other filters is [`PageError::OtherFilters`].

use crate::error::{Classify, ErrorKind};

/// The page size for `page_size = 0`, unless an RPC sets its own.
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// The largest page, unless an RPC sets its own.
pub const MAX_PAGE_SIZE: u32 = 1_000;

/// A page token that cannot be used.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PageError {
    /// Not a token this daemon issued.
    #[error("invalid page token")]
    Malformed,
    /// Issued for a request with other filters.
    #[error("the page token was issued for a request with other filters")]
    OtherFilters,
}

impl Classify for PageError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// The size of a page: `requested`, or [`DEFAULT_PAGE_SIZE`] for 0, at
/// most [`MAX_PAGE_SIZE`].
pub fn clamp_page_size(requested: u32) -> usize {
    clamp_page_size_within(requested, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE)
}

/// [`clamp_page_size`] with an RPC's own default and cap (`Trees.List`'s
/// 1,000 and 10,000).
pub fn clamp_page_size_within(requested: u32, default: u32, max: u32) -> usize {
    let size = if requested == 0 {
        default
    } else {
        requested.min(max)
    };
    usize::try_from(size).unwrap_or(usize::MAX)
}

/// A hash of a request's filters: every field that changes which items the
/// list holds, in a fixed order. Two requests list the same items exactly
/// when their hashes are equal.
pub fn filter_hash<I, S>(parts: I) -> u64
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        let part = part.as_ref();
        // Length-prefixed, so ("ab", "c") and ("a", "bc") differ.
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let mut first = [0; 8];
    first.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
    u64::from_be_bytes(first)
}

/// The token that resumes after `cursor` for a request whose filters hash
/// to `filter_hash`.
pub fn encode_token(cursor: &str, filter_hash: u64) -> String {
    let mut bytes = filter_hash.to_be_bytes().to_vec();
    bytes.extend_from_slice(cursor.as_bytes());
    base64url_encode(&bytes)
}

/// The cursor in `token`, which must have been issued for a request whose
/// filters hash to `filter_hash`. An empty token is the first page: `None`.
pub fn decode_token(token: &str, filter_hash: u64) -> Result<Option<String>, PageError> {
    if token.is_empty() {
        return Ok(None);
    }
    let bytes = base64url_decode(token).ok_or(PageError::Malformed)?;
    if bytes.len() < 8 {
        return Err(PageError::Malformed);
    }
    let (hash, cursor) = bytes.split_at(8);
    if hash != filter_hash.to_be_bytes() {
        return Err(PageError::OtherFilters);
    }
    let cursor = String::from_utf8(cursor.to_vec()).map_err(|_| PageError::Malformed)?;
    Ok(Some(cursor))
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// RFC 4648 base64url without padding.
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
        }
    }
    out
}

/// The inverse of [`base64url_encode`]; `None` for anything it would not
/// have written.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut values = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|&c| c == byte)?;
        values.push(u32::try_from(value).ok()?);
    }
    if values.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(values.len() * 3 / 4);
    for chunk in values.chunks(4) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0, |n, (i, value)| n | (value << (18 - 6 * i)));
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    // Reject non-canonical trailing bits, so each token has one spelling.
    (base64url_encode(&out) == text).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_tokens_round_trip_and_reject_other_filters() {
        let hash = filter_hash(["repo", "active", "alice"]);
        for cursor in [
            "",
            "1792175477042:aaaa000000000001",
            "é",
            "ab",
            "abc",
            "abcd",
        ] {
            let token = encode_token(cursor, hash);
            assert!(
                token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{token}"
            );
            assert_eq!(decode_token(&token, hash), Ok(Some(cursor.to_owned())));
        }
        let token = encode_token("c", hash);
        let other = filter_hash(["repo", "active", "bob"]);
        assert_eq!(decode_token(&token, other), Err(PageError::OtherFilters));
        assert_eq!(decode_token("", other), Ok(None));
        assert_eq!(
            decode_token("not a token!", hash),
            Err(PageError::Malformed)
        );
        assert_eq!(decode_token("AAAA", hash), Err(PageError::Malformed));
        assert_eq!(PageError::OtherFilters.kind(), ErrorKind::InvalidArgument);
    }

    #[test]
    fn filter_parts_are_delimited() {
        assert_ne!(filter_hash(["ab", "c"]), filter_hash(["a", "bc"]));
        assert_eq!(filter_hash(["ab", "c"]), filter_hash(["ab", "c"]));
    }

    #[test]
    fn page_sizes_default_and_cap() {
        assert_eq!(clamp_page_size(0), 100);
        assert_eq!(clamp_page_size(7), 7);
        assert_eq!(clamp_page_size(5_000), 1_000);
        assert_eq!(clamp_page_size_within(0, 1_000, 10_000), 1_000);
        assert_eq!(clamp_page_size_within(20_000, 1_000, 10_000), 10_000);
    }

    #[test]
    fn base64url_matches_rfc_4648() {
        // RFC 4648 section 10, unpadded.
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url_encode(plain.as_bytes()), encoded);
            assert_eq!(base64url_decode(encoded).unwrap(), plain.as_bytes());
        }
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url_decode("Zh"), None, "non-canonical trailing bits");
    }
}
