//! `SubscribeOperationsResponse.cursor`: every followed repository's
//! frontier, as opaque base64url (no padding) of a versioned binary
//! encoding: byte 1, then per repository its 8-byte id, a 2-byte count, and
//! that many 64-byte operation ids. Not bound to the filter.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reef_lib::jj::OperationIdHex;
use reef_lib::jj::oplog::Frontier;

use super::SubscribeError;
use crate::model::RepoId;

const VERSION: u8 = 1;
const OP_ID_BYTES: usize = 64;

/// Each followed repository's frontier, as a cursor.
pub fn encode(positions: &BTreeMap<RepoId, Frontier>) -> String {
    let mut bytes = vec![VERSION];
    for (repo, frontier) in positions {
        bytes.extend(hex_bytes(&repo.as_hex()));
        let count = u16::try_from(frontier.ids().len()).unwrap_or(u16::MAX);
        bytes.extend(count.to_be_bytes());
        for id in frontier.ids().iter().take(usize::from(count)) {
            bytes.extend(hex_bytes(id.as_str()));
        }
    }
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The frontiers a cursor holds.
pub fn decode(cursor: &str) -> Result<BTreeMap<RepoId, Frontier>, SubscribeError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.trim())
        .map_err(|_| SubscribeError::InvalidCursor)?;
    let (&version, mut rest) = bytes.split_first().ok_or(SubscribeError::InvalidCursor)?;
    if version != VERSION {
        return Err(SubscribeError::InvalidCursor);
    }
    let mut positions = BTreeMap::new();
    while !rest.is_empty() {
        let repo: RepoId = to_hex(take(&mut rest, 8)?)
            .parse()
            .map_err(|_| SubscribeError::InvalidCursor)?;
        let count = take(&mut rest, 2)?;
        let count = u16::from_be_bytes([count[0], count[1]]);
        let mut ids = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let id: OperationIdHex = to_hex(take(&mut rest, OP_ID_BYTES)?)
                .parse()
                .map_err(|_| SubscribeError::InvalidCursor)?;
            ids.push(id);
        }
        if count == 0 || positions.insert(repo, Frontier::new(ids)).is_some() {
            return Err(SubscribeError::InvalidCursor);
        }
    }
    Ok(positions)
}

fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8], SubscribeError> {
    if rest.len() < n {
        return Err(SubscribeError::InvalidCursor);
    }
    let (taken, left) = rest.split_at(n);
    *rest = left;
    Ok(taken)
}

fn hex_bytes(hex: &str) -> impl Iterator<Item = u8> + '_ {
    hex.as_bytes().chunks(2).map(|pair| {
        let digit = |b: u8| char::from(b).to_digit(16).unwrap_or(0);
        u8::try_from(digit(pair[0]) * 16 + digit(pair.get(1).copied().unwrap_or(b'0'))).unwrap_or(0)
    })
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(digit: char) -> OperationIdHex {
        digit.to_string().repeat(128).parse().unwrap()
    }

    #[test]
    fn cursors_round_trip() {
        let a: RepoId = "0123456789abcdef".parse().unwrap();
        let b: RepoId = "fedcba9876543210".parse().unwrap();
        let positions = BTreeMap::from([
            (a, Frontier::new([op('a')])),
            (b, Frontier::new([op('c'), op('b')])),
        ]);
        let cursor = encode(&positions);
        assert!(!cursor.contains('='), "no padding");
        assert_eq!(decode(&cursor).unwrap(), positions);
        assert_eq!(decode(&encode(&BTreeMap::new())).unwrap(), BTreeMap::new());
    }

    #[test]
    fn bad_cursors_are_refused() {
        let a: RepoId = "0123456789abcdef".parse().unwrap();
        let cursor = encode(&BTreeMap::from([(a, Frontier::new([op('a')]))]));
        let mut bytes = URL_SAFE_NO_PAD.decode(&cursor).unwrap();
        for bad in [
            "abc".to_owned(),
            "!!".to_owned(),
            String::new(),
            URL_SAFE_NO_PAD.encode(&bytes[..bytes.len() - 1]),
            {
                bytes[0] = 2;
                URL_SAFE_NO_PAD.encode(&bytes)
            },
        ] {
            assert!(
                matches!(decode(&bad), Err(SubscribeError::InvalidCursor)),
                "{bad:?}"
            );
        }
    }
}
