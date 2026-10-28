//! SCIP indexes, split for upload (R2.14): an encoded `scip.Index` read at
//! the wire level into its tool, its project root and its documents, each
//! kept as the encoded `scip.Document` the daemon decodes. The client
//! carries no SCIP schema; field numbers are SCIP's (`scip.proto`):
//! `Index { metadata = 1, documents = 2 }`, `Metadata { tool_info = 2,
//! project_root = 3 }`, `ToolInfo { name = 1, version = 2 }` and
//! `Document { relative_path = 1 }`. Other fields, the index's
//! `external_symbols` among them, are skipped.

use prost::bytes::Buf as _;
use prost::encoding::{WireType, decode_key, decode_varint};

/// One document of an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    /// Its `relative_path`, from the index's project root.
    pub relative_path: String,
    /// The encoded `scip.Document`.
    pub bytes: Vec<u8>,
}

/// An index, split.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    /// `metadata.tool_info.name`.
    pub tool_name: String,
    /// `metadata.tool_info.version`.
    pub tool_version: String,
    /// `metadata.project_root`, a URI such as `file:///path`.
    pub project_root: String,
    /// The documents, in order.
    pub documents: Vec<Document>,
}

/// An index that does not decode.
#[derive(Debug, thiserror::Error)]
#[error("not a SCIP index: {0}")]
pub struct SplitError(String);

#[expect(
    clippy::needless_pass_by_value,
    reason = "by value so calls can write `.or_else(fail)`"
)]
fn fail<T>(error: impl ToString) -> Result<T, SplitError> {
    Err(SplitError(error.to_string()))
}

/// The fields of one message: `(number, wire type, length-delimited bytes
/// or empty)`, the others skipped.
fn fields(mut buf: &[u8]) -> Result<Vec<(u32, &[u8])>, SplitError> {
    let mut out = Vec::new();
    while buf.has_remaining() {
        let (number, wire) = decode_key(&mut buf).or_else(fail)?;
        match wire {
            WireType::LengthDelimited => {
                let len = usize::try_from(decode_varint(&mut buf).or_else(fail)?).or_else(fail)?;
                if len > buf.len() {
                    return fail("a field runs past the end");
                }
                out.push((number, &buf[..len]));
                buf = &buf[len..];
            }
            WireType::Varint => {
                decode_varint(&mut buf).or_else(fail)?;
            }
            WireType::ThirtyTwoBit if buf.len() >= 4 => buf = &buf[4..],
            WireType::SixtyFourBit if buf.len() >= 8 => buf = &buf[8..],
            _ => return fail("an unsupported or truncated field"),
        }
    }
    Ok(out)
}

fn text(bytes: &[u8]) -> Result<String, SplitError> {
    String::from_utf8(bytes.to_vec()).or_else(fail)
}

/// Splits an encoded `scip.Index`.
pub fn split(index: &[u8]) -> Result<Index, SplitError> {
    let mut out = Index::default();
    for (number, bytes) in fields(index)? {
        match number {
            1 => {
                for (number, bytes) in fields(bytes)? {
                    match number {
                        2 => {
                            for (number, bytes) in fields(bytes)? {
                                match number {
                                    1 => out.tool_name = text(bytes)?,
                                    2 => out.tool_version = text(bytes)?,
                                    _ => {}
                                }
                            }
                        }
                        3 => out.project_root = text(bytes)?,
                        _ => {}
                    }
                }
            }
            2 => {
                let relative_path = fields(bytes)?
                    .into_iter()
                    .find(|(number, _)| *number == 1)
                    .map(|(_, path)| text(path))
                    .transpose()?
                    .unwrap_or_default();
                out.documents.push(Document {
                    relative_path,
                    bytes: bytes.to_vec(),
                });
            }
            _ => {}
        }
    }
    Ok(out)
}

/// blake3 of `bytes` in lowercase hex: a document's `content_digest`.
pub fn content_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Indexes of `sizes` grouped into batches of at most `max_count` items and
/// `max_bytes` bytes; an item larger than `max_bytes` alone is left out and
/// returned separately.
pub fn batches(
    sizes: &[usize],
    max_count: usize,
    max_bytes: usize,
) -> (Vec<Vec<usize>>, Vec<usize>) {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut too_large = Vec::new();
    let mut bytes = 0;
    for (i, &size) in sizes.iter().enumerate() {
        if size > max_bytes {
            too_large.push(i);
            continue;
        }
        match batches.last_mut() {
            Some(batch) if batch.len() < max_count && bytes + size <= max_bytes => {
                batch.push(i);
                bytes += size;
            }
            _ => {
                batches.push(vec![i]);
                bytes = size;
            }
        }
    }
    (batches, too_large)
}

#[cfg(test)]
mod tests {
    use prost::encoding::{encode_key, encode_varint};

    use super::*;

    fn field(out: &mut Vec<u8>, number: u32, bytes: &[u8]) {
        encode_key(number, WireType::LengthDelimited, out);
        encode_varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    fn message(fields_: &[(u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (number, bytes) in fields_ {
            field(&mut out, *number, bytes);
        }
        out
    }

    /// Test 11 of R2.14.
    #[test]
    fn scip_split() {
        let tool = message(&[(1, b"scip-typescript"), (2, b"0.3.14"), (3, b"--arg")]);
        let mut metadata = message(&[(2, &tool), (3, b"file:///repo/web")]);
        // An unknown varint field, skipped.
        encode_key(9, WireType::Varint, &mut metadata);
        encode_varint(7, &mut metadata);
        let docs: Vec<Vec<u8>> = ["a.ts", "b/c.ts", "d.tsx"]
            .iter()
            .map(|path| message(&[(4, b"typescript"), (1, path.as_bytes()), (2, b"\x0a\x00")]))
            .collect();
        let external = message(&[(1, b"scip-typescript npm x 1 X#")]);
        let index = message(&[
            (1, &metadata),
            (2, &docs[0]),
            (3, &external),
            (2, &docs[1]),
            (2, &docs[2]),
        ]);
        let split = split(&index).unwrap();
        assert_eq!(split.tool_name, "scip-typescript");
        assert_eq!(split.tool_version, "0.3.14");
        assert_eq!(split.project_root, "file:///repo/web");
        let paths: Vec<&str> = split
            .documents
            .iter()
            .map(|d| d.relative_path.as_str())
            .collect();
        assert_eq!(paths, ["a.ts", "b/c.ts", "d.tsx"]);
        for (document, bytes) in split.documents.iter().zip(&docs) {
            assert_eq!(&document.bytes, bytes, "documents are kept byte for byte");
        }
        assert!(
            super::split(&index[..index.len() - 3]).is_err(),
            "truncated"
        );
    }

    #[test]
    fn batches_respect_both_bounds() {
        let (batches, too_large) = batches(&[10, 10, 10, 50, 5, 100], 2, 40);
        assert_eq!(batches, [vec![0, 1], vec![2, 4]]);
        assert_eq!(too_large, [3, 5]);
    }
}
