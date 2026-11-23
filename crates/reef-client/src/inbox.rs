//! Which needs-you-now items of a `WatchInbox` stream are news (R3.39,
//! ADR 0093): the one rule `reef inbox watch` and the desktop shell share.
//!
//! The daemon numbers every needs-you-now episode (`InboxItem.raised`) and
//! says, per repository, how far a section has sent (`raised_through`). A
//! client keeps the largest it saw per repository. An item raised above it
//! is news. A repository it has no number for takes the number at the end
//! of its listing and announces nothing it listed, so a first start, an
//! upgrade or a newly registered repository is quiet. A stream's order
//! (before or after `synced`) plays no part.

use std::collections::BTreeMap;

use reef_api::v1;
use serde::{Deserialize, Serialize};

/// How far a client has handled each repository's raises.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct News {
    /// The raise ledger these numbers are from; numbers of another are
    /// meaningless.
    pub epoch: String,
    /// The largest `raised_through` handled, by repository id.
    pub through: BTreeMap<String, u64>,
}

impl News {
    /// Takes a frame's `raise_epoch`: another ledger starts every
    /// repository over, quietly. Returns whether it did.
    pub fn epoch(&mut self, epoch: &str) -> bool {
        if self.epoch == epoch {
            return false;
        }
        epoch.clone_into(&mut self.epoch);
        self.through.clear();
        true
    }

    /// The needs-you-now items of `section` that are news, and moves the
    /// repository's number past what the section completes.
    pub fn section<'a>(&mut self, section: &'a v1::InboxSectionChange) -> Vec<&'a v1::InboxItem> {
        if section.removed {
            self.through.remove(&section.repo_id);
            return Vec::new();
        }
        let Some(through) = self.through.get_mut(&section.repo_id) else {
            if section.listing() == v1::InboxListing::End {
                self.through
                    .insert(section.repo_id.clone(), section.raised_through);
            }
            return Vec::new();
        };
        let handled = *through;
        *through = handled.max(section.raised_through);
        section
            .items
            .iter()
            .filter(|item| item.bucket() == v1::InboxBucket::NeedsYouNow && item.raised > handled)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needs(key: &str, raised: u64) -> v1::InboxItem {
        v1::InboxItem {
            key: key.to_owned(),
            bucket: v1::InboxBucket::NeedsYouNow.into(),
            raised,
            ..v1::InboxItem::default()
        }
    }

    fn section(
        repo: &str,
        listing: v1::InboxListing,
        items: Vec<v1::InboxItem>,
        raised_through: u64,
    ) -> v1::InboxSectionChange {
        v1::InboxSectionChange {
            repo_id: repo.to_owned(),
            items,
            listing: listing.into(),
            raised_through,
            ..v1::InboxSectionChange::default()
        }
    }

    fn keys(items: &[&v1::InboxItem]) -> Vec<String> {
        items.iter().map(|item| item.key.clone()).collect()
    }

    use v1::InboxListing::{End, Part, Unspecified};

    const NONE: Vec<&v1::InboxItem> = Vec::new();

    #[test]
    fn a_listing_is_quiet_and_what_follows_is_news() {
        let mut news = News::default();
        assert!(news.epoch("e"));
        let listed = section("b", End, vec![needs("old", 3)], 3);
        assert_eq!(news.section(&listed), NONE, "the starting inbox");
        let later = section("b", Unspecified, vec![needs("new", 9), needs("old", 3)], 9);
        assert_eq!(keys(&news.section(&later)), ["new"]);
        let again = section("b", Unspecified, vec![needs("new", 9)], 9);
        assert_eq!(
            news.section(&again),
            NONE,
            "a changed item is not news twice"
        );
    }

    #[test]
    fn each_repository_is_judged_on_its_own() {
        let mut news = News::default();
        news.epoch("e");
        news.section(&section("b", End, vec![], 0));
        let b = section("b", Unspecified, vec![needs("b1", 4)], 4);
        assert_eq!(keys(&news.section(&b)), ["b1"]);
        let a = section("a", End, vec![needs("a-old", 2)], 4);
        assert_eq!(news.section(&a), NONE);
    }

    #[test]
    fn a_reconnect_announces_what_arrived_while_away() {
        let mut news = News::default();
        news.epoch("e");
        news.section(&section("b", End, vec![needs("old", 3)], 3));
        let first = section("b", Part, vec![needs("old", 3)], 0);
        let last = section("b", End, vec![needs("away", 5)], 5);
        assert_eq!(news.section(&first), NONE);
        assert_eq!(keys(&news.section(&last)), ["away"]);
    }

    #[test]
    fn an_unread_repository_waits_for_its_listing() {
        let mut news = News::default();
        news.epoch("e");
        let failing = section("b", Unspecified, vec![], 0);
        assert_eq!(news.section(&failing), NONE);
        let listed = section("b", End, vec![needs("old", 3)], 3);
        assert_eq!(
            news.section(&listed),
            NONE,
            "its first good read is its starting inbox"
        );
    }

    #[test]
    fn another_ledger_or_a_removed_repository_starts_over() {
        let mut news = News::default();
        news.epoch("e");
        news.section(&section("b", End, vec![], 7));
        assert!(!news.epoch("e"));
        assert!(news.epoch("restored"));
        assert_eq!(
            news.section(&section("b", Unspecified, vec![needs("x", 2)], 2)),
            NONE
        );
        news.section(&section("c", End, vec![], 1));
        let removed = v1::InboxSectionChange {
            repo_id: "c".to_owned(),
            removed: true,
            ..v1::InboxSectionChange::default()
        };
        news.section(&removed);
        assert!(!news.through.contains_key("c"));
    }

    #[test]
    fn watching_items_are_never_news() {
        let mut news = News::default();
        news.epoch("e");
        news.section(&section("b", End, vec![], 0));
        let watching = v1::InboxItem {
            key: "w".to_owned(),
            bucket: v1::InboxBucket::Watching.into(),
            ..v1::InboxItem::default()
        };
        assert_eq!(
            news.section(&section("b", Unspecified, vec![watching], 0)),
            NONE
        );
    }
}
