//! What every `WatchInbox` stream shares (#372, ADR 0091): one immutable
//! snapshot per repository of its placed items, replaced whole after each
//! read, and the difference between two snapshots.
//!
//! A trigger never edits a snapshot: it marks the repository for a read,
//! and the read's result replaces the snapshot. A stream holds the snapshot
//! it last sent and sends [`diff`] against the newest, so a slow stream
//! skips intermediate states and nothing queues.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use prost::Message as _;
use reef_api::v1;

use super::{Buckets, UNCLAIMED_AFTER_MS, time};
use crate::model::{RepoId, Timestamp};

/// Items and keys one frame holds at most.
pub(crate) const FRAME_ENTRIES: usize = 1_000;
/// Encoded bytes one frame holds at most; one item is never split. Small
/// enough that a slow link (R3.40) has a whole frame before the web UI calls
/// the stream silent, 20 s at the least.
pub(crate) const FRAME_BYTES: usize = 128 << 10;

/// One repository's inbox as its last read left it, without the digest,
/// which depends on the viewer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RepoBoard {
    /// Larger is newer, across every repository.
    pub rev: u64,
    /// Both buckets' listed items, each cut at `BUCKET_CAP`, by key.
    pub items: BTreeMap<String, v1::InboxItem>,
    pub needs_you_now_total: u32,
    pub watching_total: u32,
    /// Every key in either bucket, the ones past the cap included: the
    /// raise ledger keeps a row while its key is here. No stream sees it.
    pub keys: BTreeSet<String>,
    /// Set while the repository fails to read; `items` are the last good
    /// read's.
    pub error: Option<v1::InboxError>,
    /// The first lines of the repository's newest landed changes, by change
    /// id: every viewer's newest landings are among them.
    pub subjects: Arc<HashMap<String, String>>,
    /// When an item's words change with no event: a pending instruction's
    /// target turns unseen for `UNCLAIMED_AFTER_MS`.
    pub restale_at: Option<Timestamp>,
    /// False for a read that is no listing: a failed first read, whose items
    /// are empty, or a first read whose raise ledger write failed, whose
    /// items carry no numbers yet.
    pub read: bool,
    /// The newest raise of the repository; never lower than the last board's.
    pub raised_through: u64,
    /// These raises are the ledger's. False while a failed write holds them
    /// at 0, so the next read writes instead of taking them.
    pub settled: bool,
}

impl RepoBoard {
    /// A good read of `repo`; its deadline is from the time its words were
    /// read at.
    pub(crate) fn read(
        repo: RepoId,
        buckets: Buckets,
        subjects: Arc<HashMap<String, String>>,
    ) -> RepoBoard {
        let now = buckets.now;
        let total = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let items: BTreeMap<String, v1::InboxItem> = buckets
            .needs_you_now
            .into_iter()
            .chain(buckets.watching)
            .map(|item| {
                let message = item.message(repo);
                (message.key.clone(), message)
            })
            .collect();
        let restale_at = items
            .values()
            .filter_map(|item| unclaimed_at(item, now))
            .min();
        RepoBoard {
            rev: 0,
            items,
            needs_you_now_total: total(buckets.needs_you_now_total),
            watching_total: total(buckets.watching_total),
            keys: buckets.keys,
            error: None,
            subjects,
            restale_at,
            read: true,
            raised_through: 0,
            settled: false,
        }
    }

    /// A failed read: the last good items stand, with `error`.
    pub(crate) fn failed(last: Option<&RepoBoard>, error: v1::InboxError) -> RepoBoard {
        let mut board = last.cloned().unwrap_or_else(|| RepoBoard {
            rev: 0,
            items: BTreeMap::new(),
            needs_you_now_total: 0,
            watching_total: 0,
            keys: BTreeSet::new(),
            error: None,
            subjects: Arc::default(),
            restale_at: None,
            read: false,
            raised_through: 0,
            settled: false,
        });
        board.error = Some(error);
        // The retry governs when a failing repository is read again.
        board.restale_at = None;
        board
    }

    /// Whether a stream could tell `self` from `other`; `rev` aside.
    pub(crate) fn same(&self, other: &RepoBoard) -> bool {
        self.items == other.items
            && self.needs_you_now_total == other.needs_you_now_total
            && self.watching_total == other.watching_total
            && self.error == other.error
            && self.subjects == other.subjects
            && self.read == other.read
            && self.raised_through == other.raised_through
    }
}

/// When a pending instruction's headline turns to "nobody has picked it
/// up", if that is still ahead of `now` (`words::comment`).
fn unclaimed_at(item: &v1::InboxItem, now: Timestamp) -> Option<Timestamp> {
    let Some(v1::inbox_item::Record::Comment(comment)) = &item.record else {
        return None;
    };
    let instruction = comment.instruction.as_ref()?;
    if instruction.state() != v1::InstructionState::Pending {
        return None;
    }
    let seen = instruction.target.as_ref()?.last_seen_at.as_ref()?;
    let at = Timestamp(time(Some(seen)).0 + UNCLAIMED_AFTER_MS + 1);
    (at > now).then_some(at)
}

/// What turns `sent` into `now`: the items that entered or changed (moved
/// to the other bucket included), newest first, and the keys that left.
/// `sent` is `None` for a repository the stream has not sent yet.
pub(crate) fn diff(sent: Option<&RepoBoard>, now: &RepoBoard) -> (Vec<v1::InboxItem>, Vec<String>) {
    let mut items: Vec<v1::InboxItem> = now
        .items
        .iter()
        .filter(|(key, item)| sent.and_then(|sent| sent.items.get(*key)) != Some(*item))
        .map(|(_, item)| item.clone())
        .collect();
    items.sort_by(|a, b| {
        let at = |item: &v1::InboxItem| time(item.since.as_ref());
        at(b).cmp(&at(a)).then_with(|| a.key.cmp(&b.key))
    });
    let left = sent.map_or_else(Vec::new, |sent| {
        sent.items
            .keys()
            .filter(|key| !now.items.contains_key(*key))
            .cloned()
            .collect()
    });
    (items, left)
}

/// `changes` as frames of at most [`FRAME_ENTRIES`] items and keys and
/// [`FRAME_BYTES`]. A change too large for one frame continues in the next
/// with its state repeated, so every frame's sections read alike. A frame
/// takes at least one item or key, so a state over the bound still ends.
pub(crate) fn frames(changes: Vec<v1::InboxSectionChange>) -> Vec<Vec<v1::InboxSectionChange>> {
    let mut frames: Vec<Vec<v1::InboxSectionChange>> = Vec::new();
    let mut frame: Vec<v1::InboxSectionChange> = Vec::new();
    let (mut entries, mut bytes) = (0, 0);
    for change in changes {
        let mut rest = change;
        loop {
            let state = v1::InboxSectionChange {
                items: Vec::new(),
                left: Vec::new(),
                ..rest.clone()
            };
            let head = state.encoded_len();
            if !frame.is_empty() && (entries >= FRAME_ENTRIES || bytes + head > FRAME_BYTES) {
                frames.push(std::mem::take(&mut frame));
                (entries, bytes) = (0, 0);
            }
            let mut part = state;
            bytes += head;
            let mut items = std::mem::take(&mut rest.items).into_iter().peekable();
            while let Some(item) = items.peek() {
                let size = item.encoded_len() + 4;
                let first = frame.is_empty() && part.items.is_empty() && part.left.is_empty();
                if !first && (entries >= FRAME_ENTRIES || bytes + size > FRAME_BYTES) {
                    break;
                }
                bytes += size;
                entries += 1;
                part.items.extend(items.next());
            }
            rest.items = items.collect();
            let mut left = std::mem::take(&mut rest.left).into_iter().peekable();
            if rest.items.is_empty() {
                while let Some(key) = left.peek() {
                    let size = key.len() + 4;
                    let first = frame.is_empty() && part.items.is_empty() && part.left.is_empty();
                    if !first && (entries >= FRAME_ENTRIES || bytes + size > FRAME_BYTES) {
                        break;
                    }
                    bytes += size;
                    entries += 1;
                    part.left.extend(left.next());
                }
            }
            rest.left = left.collect();
            let last = rest.items.is_empty() && rest.left.is_empty();
            if !last {
                part.raised_through = 0;
                if part.listing() == v1::InboxListing::End {
                    part.set_listing(v1::InboxListing::Part);
                }
            }
            frame.push(part);
            if last {
                break;
            }
            frames.push(std::mem::take(&mut frame));
            (entries, bytes) = (0, 0);
        }
    }
    if !frame.is_empty() {
        frames.push(frame);
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, bucket: v1::InboxBucket, since: i64, headline: &str) -> v1::InboxItem {
        v1::InboxItem {
            key: key.to_owned(),
            headline: headline.to_owned(),
            since: Some(prost_types::Timestamp {
                seconds: since,
                nanos: 0,
            }),
            bucket: bucket.into(),
            ..v1::InboxItem::default()
        }
    }

    fn board(items: &[v1::InboxItem]) -> RepoBoard {
        RepoBoard {
            rev: 1,
            items: items
                .iter()
                .map(|item| (item.key.clone(), item.clone()))
                .collect(),
            needs_you_now_total: 0,
            watching_total: 0,
            keys: items.iter().map(|item| item.key.clone()).collect(),
            error: None,
            subjects: Arc::default(),
            restale_at: None,
            read: true,
            raised_through: 0,
            settled: true,
        }
    }

    /// `sent` with `diff`'s items put and keys removed: what a client holds.
    fn fold(sent: &RepoBoard, (items, left): (Vec<v1::InboxItem>, Vec<String>)) -> RepoBoard {
        let mut held = sent.clone();
        for key in left {
            held.items.remove(&key);
        }
        for item in items {
            held.items.insert(item.key.clone(), item);
        }
        held
    }

    use v1::InboxBucket::{NeedsYouNow, Watching};

    #[test]
    fn entered_changed_moved_and_left() {
        let sent = board(&[
            item("help:r:1", NeedsYouNow, 10, "asks"),
            item("proposal:r:a", Watching, 20, "in review"),
            item("proposal:r:b", Watching, 30, "in review"),
        ]);
        let now = board(&[
            item("help:r:1", NeedsYouNow, 10, "asks"),
            item("proposal:r:a", NeedsYouNow, 25, "needs a sign-off"),
            item("proposal:r:b", Watching, 30, "in review, rebased"),
            item("help:r:2", NeedsYouNow, 40, "asks"),
        ]);
        let (items, left) = diff(Some(&sent), &now);
        let keys: Vec<&str> = items.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(
            keys,
            ["help:r:2", "proposal:r:b", "proposal:r:a"],
            "newest first"
        );
        assert_eq!(left, Vec::<String>::new());
        assert_eq!(items[2].bucket(), NeedsYouNow, "moved, with its new bucket");
        let gone = board(&[item("help:r:2", NeedsYouNow, 40, "asks")]);
        let (items, left) = diff(Some(&now), &gone);
        assert_eq!(items, Vec::<v1::InboxItem>::new());
        assert_eq!(left, ["help:r:1", "proposal:r:a", "proposal:r:b"]);
    }

    #[test]
    fn a_first_send_lists_everything_and_an_unchanged_board_nothing() {
        let now = board(&[item("help:r:1", NeedsYouNow, 10, "asks")]);
        let (items, left) = diff(None, &now);
        assert_eq!(items.len(), 1);
        assert_eq!(left, Vec::<String>::new());
        assert_eq!(diff(Some(&now), &now), (Vec::new(), Vec::new()));
    }

    #[test]
    fn folding_the_diff_gives_the_newer_board() {
        // Every pair of boards over four keys, each absent or in one of two
        // states: the fold of the diff is the newer board.
        let states = |pick: usize| -> Vec<v1::InboxItem> {
            (0..4)
                .filter_map(|n| match (pick / 3usize.pow(n)) % 3 {
                    0 => None,
                    1 => Some(item(&format!("help:r:{n}"), NeedsYouNow, i64::from(n), "a")),
                    _ => Some(item(&format!("help:r:{n}"), Watching, i64::from(n), "b")),
                })
                .collect()
        };
        for a in 0..81 {
            for b in 0..81 {
                let (old, new) = (board(&states(a)), board(&states(b)));
                assert_eq!(
                    fold(&old, diff(Some(&old), &new)).items,
                    new.items,
                    "{a} -> {b}"
                );
            }
        }
    }

    #[test]
    fn a_failed_read_keeps_the_last_good_items() {
        let good = board(&[item("help:r:1", NeedsYouNow, 10, "asks")]);
        let error = v1::InboxError {
            reason: "REPOSITORY_UNAVAILABLE".to_owned(),
            message: "the repository could not be opened".to_owned(),
        };
        let failed = RepoBoard::failed(Some(&good), error.clone());
        assert_eq!(failed.error, Some(error));
        assert!(!failed.same(&good), "a stream sees the error");
        assert_eq!(
            diff(Some(&good), &failed),
            (Vec::new(), Vec::new()),
            "no item leaves"
        );
        let recovered = board(&[item("help:r:2", NeedsYouNow, 20, "asks")]);
        let (items, left) = diff(Some(&failed), &recovered);
        assert_eq!((items.len(), left), (1, vec!["help:r:1".to_owned()]));
    }

    #[test]
    fn a_pending_instruction_restales_when_its_target_turns_unseen() {
        let seen = 1_000_000;
        let comment = v1::Comment {
            instruction: Some(v1::Instruction {
                state: v1::InstructionState::Pending.into(),
                target: Some(v1::InstructionTarget {
                    last_seen_at: Some(prost_types::Timestamp {
                        seconds: seen,
                        nanos: 0,
                    }),
                    ..v1::InstructionTarget::default()
                }),
                ..v1::Instruction::default()
            }),
            ..v1::Comment::default()
        };
        let pending = v1::InboxItem {
            record: Some(v1::inbox_item::Record::Comment(comment)),
            ..v1::InboxItem::default()
        };
        let due = Timestamp(seen * 1000 + UNCLAIMED_AFTER_MS + 1);
        let a_minute_before = Timestamp(seen * 1000 + UNCLAIMED_AFTER_MS - 60_000);
        assert_eq!(unclaimed_at(&pending, a_minute_before), Some(due));
        assert_eq!(unclaimed_at(&pending, due), None, "already stale");
    }

    #[test]
    fn crossing_the_cap_enters_one_and_leaves_one() {
        let listed = |from: i64| -> Vec<v1::InboxItem> {
            (from..from + 200)
                .map(|n| item(&format!("help:r:{n:03}"), NeedsYouNow, n, "asks"))
                .collect()
        };
        let mut sent = board(&listed(0));
        sent.needs_you_now_total = 200;
        // Item 201 is the newest; the oldest falls past the cap.
        let mut now = board(&listed(1));
        now.needs_you_now_total = 201;
        let (items, left) = diff(Some(&sent), &now);
        assert_eq!(
            items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["help:r:200"]
        );
        assert_eq!(left, ["help:r:000"]);
    }

    #[test]
    fn a_large_change_is_cut_across_frames_with_its_state() {
        let items: Vec<v1::InboxItem> = (0..2_500)
            .map(|n| item(&format!("help:r:{n:04}"), NeedsYouNow, n, "asks"))
            .collect();
        let change = v1::InboxSectionChange {
            repo_id: "r".to_owned(),
            items,
            left: (0..10).map(|n| format!("gone:{n}")).collect(),
            needs_you_now_total: 2_500,
            ..v1::InboxSectionChange::default()
        };
        let small = v1::InboxSectionChange {
            repo_id: "s".to_owned(),
            left: vec!["gone:s".to_owned()],
            ..v1::InboxSectionChange::default()
        };
        let frames = frames(vec![change, small]);
        assert_eq!(frames.len(), 3);
        for frame in &frames {
            let entries: usize = frame.iter().map(|s| s.items.len() + s.left.len()).sum();
            assert!(entries <= FRAME_ENTRIES, "{entries}");
            assert!(
                frame
                    .iter()
                    .all(|s| s.repo_id != "r" || s.needs_you_now_total == 2_500)
            );
        }
        let all: Vec<&v1::InboxSectionChange> = frames.iter().flatten().collect();
        let items: usize = all.iter().map(|s| s.items.len()).sum();
        let left: usize = all.iter().map(|s| s.left.len()).sum();
        assert_eq!((items, left), (2_500, 11));
        assert_eq!(frames[2].last().map(|s| s.repo_id.as_str()), Some("s"));
    }

    #[test]
    fn a_state_over_the_bound_still_ends() {
        for len in [127 << 10, 129 << 10, 2 << 20] {
            let change = v1::InboxSectionChange {
                repo_id: "r".to_owned(),
                items: (0..3)
                    .map(|n| item(&format!("help:r:{n}"), NeedsYouNow, n, "asks"))
                    .collect(),
                left: (0..3).map(|n| format!("gone:{n}")).collect(),
                error: Some(v1::InboxError {
                    message: "ö".repeat(len / 2),
                    ..v1::InboxError::default()
                }),
                ..v1::InboxSectionChange::default()
            };
            for items in [change.items.clone(), Vec::new()] {
                let frames = frames(vec![v1::InboxSectionChange {
                    items: items.clone(),
                    ..change.clone()
                }]);
                assert!(frames.len() <= 6, "{len}: {}", frames.len());
                let parts: Vec<_> = frames.into_iter().flatten().collect();
                let sent: Vec<_> = parts.iter().flat_map(|p| p.items.clone()).collect();
                let left: Vec<_> = parts.iter().flat_map(|p| p.left.clone()).collect();
                assert_eq!((sent, left), (items, change.left.clone()));
            }
        }
    }

    #[test]
    fn a_slow_link_has_a_frame_in_time() {
        let words = "why ".repeat(1_000);
        let items: Vec<v1::InboxItem> = (0..200)
            .map(|n| item(&format!("help:r:{n:04}"), NeedsYouNow, n, &words))
            .collect();
        let frames = frames(vec![v1::InboxSectionChange {
            repo_id: "r".to_owned(),
            items,
            ..v1::InboxSectionChange::default()
        }]);
        for sections in &frames {
            let frame = v1::WatchInboxResponse {
                sections: sections.clone(),
                ..v1::WatchInboxResponse::default()
            };
            // 20 s at 6.6 kB/s.
            assert!(frame.encoded_len() <= 132_000, "{}", frame.encoded_len());
        }
        let items: usize = frames.iter().flatten().map(|s| s.items.len()).sum();
        assert_eq!(items, 200);
    }

    #[test]
    fn only_a_listings_last_part_ends_it_and_carries_its_raises() {
        let items: Vec<v1::InboxItem> = (0..2_500)
            .map(|n| item(&format!("help:r:{n:04}"), NeedsYouNow, n, "asks"))
            .collect();
        let listing = v1::InboxSectionChange {
            repo_id: "r".to_owned(),
            items,
            listing: v1::InboxListing::End.into(),
            raised_through: 7,
            ..v1::InboxSectionChange::default()
        };
        let parts: Vec<v1::InboxSectionChange> =
            frames(vec![listing]).into_iter().flatten().collect();
        assert_eq!(parts.len(), 3);
        let shape: Vec<(v1::InboxListing, u64)> = parts
            .iter()
            .map(|part| (part.listing(), part.raised_through))
            .collect();
        assert_eq!(
            shape,
            [
                (v1::InboxListing::Part, 0),
                (v1::InboxListing::Part, 0),
                (v1::InboxListing::End, 7)
            ]
        );
    }
}
