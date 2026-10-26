//! Pagination for every List RPC (ADR 0029): `page_size` (0 means 100,
//! above 1,000 is 1,000, negative is `INVALID_ARGUMENT`), and opaque page
//! tokens bound to the request's other fields.
//!
//! A token is base64url, without padding, of an internal protobuf message:
//! a version, the operation the first page was read at (for lists over jj
//! state, from R2.02), 8 bytes of blake3 over the request with its page
//! fields cleared, and the cursor: the sort key of the last item returned.
//! Items are returned in ascending key order, so the next page is the items
//! whose key is greater than the cursor, which holds even when items were
//! added or removed between pages.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use prost::Message as _;
use reef_lib::error::{Classify, ErrorKind};

/// Page size when the request says 0.
pub const DEFAULT_PAGE_SIZE: usize = 100;
/// The largest page; a larger request is clamped to it.
pub const MAX_PAGE_SIZE: usize = 1000;
/// The token format's version.
const TOKEN_VERSION: u32 = 1;
/// A page stops before its encoded response would pass this, under tonic's
/// 4 MiB message limit (R2.02); it may then hold fewer than `page_size`
/// items.
pub const MAX_PAGE_BYTES: usize = 3 << 20;

/// A page token that does not decode, or was sent with different request
/// fields than the page it came from: reason `INVALID_PAGE_TOKEN`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PageTokenError {
    /// Not a token this daemon wrote.
    #[error("page token is not valid")]
    Malformed,
    /// A token from a request with other filters.
    #[error(
        "page token belongs to a request with different fields; send the same fields as the first page"
    )]
    Mismatch,
}

impl Classify for PageTokenError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// A negative page size.
#[derive(Debug, thiserror::Error)]
#[error("page_size must not be negative (got {0})")]
pub struct NegativePageSize(pub i32);

impl Classify for NegativePageSize {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// The internal token message.
#[derive(Clone, PartialEq, prost::Message)]
struct Token {
    #[prost(uint32, tag = "1")]
    version: u32,
    #[prost(string, tag = "2")]
    at_operation: String,
    #[prost(bytes = "vec", tag = "3")]
    filter_digest: Vec<u8>,
    #[prost(string, tag = "4")]
    cursor: String,
}

/// A request's page: how many items, after which key, and for lists over
/// jj state (R2.02) the state the first page pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
    size: usize,
    after: Option<String>,
    pinned: Option<String>,
    digest: [u8; 8],
}

impl PageRequest {
    /// The page `page_size` and `page_token` ask for. `filters` is the
    /// request with its page fields cleared: a token is only good for the
    /// RPC (`method`) and the filters it was issued for.
    pub fn new<M: prost::Message>(
        method: &str,
        page_size: i32,
        page_token: &str,
        filters: &M,
    ) -> Result<PageRequest, PageError> {
        let size = match usize::try_from(page_size) {
            Err(_) => return Err(PageError::Size(NegativePageSize(page_size))),
            Ok(0) => DEFAULT_PAGE_SIZE,
            Ok(size) => size.min(MAX_PAGE_SIZE),
        };
        let mut hasher = blake3::Hasher::new();
        hasher.update(method.as_bytes());
        hasher.update(&[0]);
        hasher.update(&filters.encode_to_vec());
        let mut digest = [0; 8];
        digest.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
        let (after, pinned) = if page_token.is_empty() {
            (None, None)
        } else {
            let bytes = URL_SAFE_NO_PAD
                .decode(page_token)
                .map_err(|_| PageTokenError::Malformed)?;
            let token = Token::decode(bytes.as_slice()).map_err(|_| PageTokenError::Malformed)?;
            if token.version != TOKEN_VERSION || token.cursor.is_empty() {
                return Err(PageTokenError::Malformed.into());
            }
            if token.filter_digest != digest {
                return Err(PageTokenError::Mismatch.into());
            }
            let pinned = Some(token.at_operation).filter(|pinned| !pinned.is_empty());
            (Some(token.cursor), pinned)
        };
        Ok(PageRequest {
            size,
            after,
            pinned,
            digest,
        })
    }

    /// Items per page.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The previous page's cursor; `None` on the first page.
    pub fn cursor(&self) -> Option<&str> {
        self.after.as_deref()
    }

    /// The state the first page pinned (an operation id, or operation
    /// heads); `None` on the first page.
    pub fn pinned(&self) -> Option<&str> {
        self.pinned.as_deref()
    }

    /// The token for the page after `cursor`, read at `pinned`.
    pub fn token_at(&self, cursor: &str, pinned: &str) -> String {
        let token = Token {
            version: TOKEN_VERSION,
            at_operation: pinned.to_owned(),
            filter_digest: self.digest.to_vec(),
            cursor: cursor.to_owned(),
        };
        URL_SAFE_NO_PAD.encode(token.encode_to_vec())
    }

    /// The page of `items` this request asks for, and the token for the
    /// next page (empty on the last). `key` gives each item's sort key;
    /// the page is in ascending key order, whatever order `items` is in.
    pub fn page<T>(&self, mut items: Vec<T>, key: impl Fn(&T) -> String) -> (Vec<T>, String) {
        let mut keyed: Vec<(String, T)> = items.drain(..).map(|item| (key(&item), item)).collect();
        keyed.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut rest = keyed
            .into_iter()
            .filter(|(key, _)| self.after.as_ref().is_none_or(|after| key > after));
        let page: Vec<(String, T)> = rest.by_ref().take(self.size).collect();
        let next = match (rest.next(), page.last()) {
            (Some(_), Some((last, _))) => self.token(last),
            _ => String::new(),
        };
        (page.into_iter().map(|(_, item)| item).collect(), next)
    }

    fn token(&self, cursor: &str) -> String {
        self.token_at(cursor, "")
    }
}

/// Why a page request is refused.
#[derive(Debug, thiserror::Error)]
pub enum PageError {
    /// A negative page size.
    #[error(transparent)]
    Size(#[from] NegativePageSize),
    /// A page token that does not fit the request.
    #[error(transparent)]
    Token(#[from] PageTokenError),
}

impl Classify for PageError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// A sort key in ascending order of `millis` (non-negative), then `id`.
pub fn oldest_first(millis: i64, id: &str) -> String {
    format!("{:019}:{id}", millis.max(0))
}

/// A sort key in descending order of `millis`, then descending `id` (a hex
/// id, each digit complemented), so ties come out as Phase 1 listed them.
pub fn newest_first(millis: i64, id: &str) -> String {
    let id: String = id
        .chars()
        .map(|digit| {
            digit
                .to_digit(16)
                .and_then(|value| char::from_digit(15 - value, 16))
                .unwrap_or(digit)
        })
        .collect();
    format!("{:019}:{id}", i64::MAX - millis.max(0))
}

#[cfg(test)]
mod tests {
    use reef_api::v1::ListWorkspacesRequest;

    use super::*;
    use crate::error::{detail_of, status};

    fn filters(owner: &str) -> ListWorkspacesRequest {
        ListWorkspacesRequest {
            owner: owner.to_owned(),
            ..ListWorkspacesRequest::default()
        }
    }

    #[test]
    fn pages_walk_every_item_once() {
        let items: Vec<u32> = (0..5).collect();
        let key = |n: &u32| format!("{n:03}");
        let mut token = String::new();
        let mut seen = Vec::new();
        loop {
            let request = PageRequest::new("/test/List", 2, &token, &filters("a")).unwrap();
            let (page, next) = request.page(items.clone(), key);
            assert!(page.len() <= 2);
            seen.extend(page);
            if next.is_empty() {
                break;
            }
            token = next;
        }
        assert_eq!(seen, items);
    }

    #[test]
    fn sizes_default_clamp_and_refuse_negatives() {
        let request = PageRequest::new("/test/List", 0, "", &filters("a")).unwrap();
        assert_eq!(request.size, DEFAULT_PAGE_SIZE);
        let request = PageRequest::new("/test/List", 5000, "", &filters("a")).unwrap();
        assert_eq!(request.size, MAX_PAGE_SIZE);
        assert!(matches!(
            PageRequest::new("/test/List", -1, "", &filters("a")),
            Err(PageError::Size(_))
        ));
    }

    #[test]
    fn tokens_are_bound_to_the_method() {
        let request = PageRequest::new("/a/ListThings", 1, "", &filters("")).unwrap();
        let (_, next) = request.page(vec![1, 2], u32::to_string);
        let error = PageRequest::new("/a/ListOthers", 1, &next, &filters("")).unwrap_err();
        assert!(matches!(error, PageError::Token(PageTokenError::Mismatch)));
    }

    #[test]
    fn tokens_are_bound_to_the_filters() {
        let request = PageRequest::new("/test/List", 1, "", &filters("a")).unwrap();
        let (_, next) = request.page(vec![1, 2], u32::to_string);
        assert_ne!(next, "");
        let error = PageRequest::new("/test/List", 1, &next, &filters("b")).unwrap_err();
        assert!(matches!(error, PageError::Token(PageTokenError::Mismatch)));
        let detail = detail_of(&status(error)).unwrap();
        assert_eq!(detail.reason, reef_api::error::INVALID_PAGE_TOKEN);
        let garbage = PageRequest::new("/test/List", 1, "!!", &filters("a")).unwrap_err();
        assert!(matches!(
            garbage,
            PageError::Token(PageTokenError::Malformed)
        ));
    }

    #[test]
    fn tokens_carry_the_pinned_state() {
        let request = PageRequest::new("/test/List", 2, "", &filters("a")).unwrap();
        assert_eq!((request.cursor(), request.pinned()), (None, None));
        let token = request.token_at("10:abcd", "00ff");
        let next = PageRequest::new("/test/List", 2, &token, &filters("a")).unwrap();
        assert_eq!(next.cursor(), Some("10:abcd"));
        assert_eq!(next.pinned(), Some("00ff"));
        assert_eq!(next.size(), 2);
        // R2.01's tokens, which pin nothing, still decode.
        let (_, old) = request.page(vec![1, 2, 3], u32::to_string);
        let next = PageRequest::new("/test/List", 2, &old, &filters("a")).unwrap();
        assert_eq!((next.cursor(), next.pinned()), (Some("2"), None));
    }

    #[test]
    fn newest_first_sorts_descending() {
        assert!(newest_first(2_000, "a") < newest_first(1_000, "a"));
        assert!(newest_first(1_000, "b") < newest_first(1_000, "a"));
        assert!(oldest_first(1_000, "z") < oldest_first(2_000, "a"));
    }
}
