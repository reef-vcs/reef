//! The raise ledger's rule (R3.39, ADR 0093). An episode is an item's key,
//! its reason and a cause: a proposal's or a conflict's commit, since a
//! sign-off binds the tree, and nothing for the other objects. A proposal
//! the automatic service has not judged since the daemon started (no status,
//! or `Pending`) reads its reason from its lifecycle, which can differ from
//! its last judgment; that reading is provisional and keeps the number its
//! commit holds, so a restart announces nothing; with no number for its
//! commit it reads 0 and writes nothing, and its first judgment numbers it. Only the
//! board task writes the ledger (`settle`); `GetInbox` reads it (`lookup`).

use std::collections::BTreeSet;

use reef_api::v1;

use super::board::RepoBoard;
use crate::db::inbox::Episode;
use crate::db::{Db, DbError};
use crate::model::RepoId;

pub(crate) fn episode(item: &v1::InboxItem) -> Option<Episode> {
    if item.bucket() != v1::InboxBucket::NeedsYouNow {
        return None;
    }
    let cause = match &item.record {
        Some(v1::inbox_item::Record::Proposal(_) | v1::inbox_item::Record::Assignment(_)) => {
            item.commit_id.clone()
        }
        _ => String::new(),
    };
    Some(Episode {
        key: item.key.clone(),
        reason: item.reason,
        provisional: matches!(
            &item.record,
            Some(v1::inbox_item::Record::Proposal(entry))
                if entry.automatic.as_ref().is_none_or(
                    |status| status.state() == v1::AutomaticState::Pending
                )
        ),
        cause,
    })
}

fn episodes(board: &RepoBoard) -> Vec<Episode> {
    board.items.values().filter_map(episode).collect()
}

fn whole(board: &RepoBoard) -> bool {
    let listed = |bucket| {
        board
            .items
            .values()
            .filter(|item| item.bucket() == bucket)
            .count()
    };
    usize::try_from(board.needs_you_now_total)
        .is_ok_and(|total| total == listed(v1::InboxBucket::NeedsYouNow))
        && usize::try_from(board.watching_total)
            .is_ok_and(|total| total == listed(v1::InboxBucket::Watching))
}

pub(crate) fn settle(
    db: &Db,
    repo: RepoId,
    board: &mut RepoBoard,
    last: Option<&RepoBoard>,
) -> Result<(), DbError> {
    let episodes = episodes(board);
    // The same read as the last takes its numbers, unless it can prune what
    // the last, cut at a bucket's cap, could not.
    if let Some(last) = last.filter(|last| last.read && last.error.is_none())
        && last.items.keys().eq(board.items.keys())
        && self::episodes(last) == episodes
        && (whole(last) || !whole(board))
    {
        for (key, item) in &mut board.items {
            item.raised = last.items.get(key).map_or(0, |last| last.raised);
        }
        board.raised_through = last.raised_through;
        return Ok(());
    }
    let listed: Option<BTreeSet<String>> =
        whole(board).then(|| board.items.keys().cloned().collect());
    let (raises, through) = db.settle_inbox_raises(repo, &episodes, listed.as_ref())?;
    number(board, &episodes, &raises);
    board.raised_through = through.max(last.map_or(0, |last| last.raised_through));
    Ok(())
}

pub(crate) fn lookup(db: &Db, repo: RepoId, items: &mut [v1::InboxItem]) -> Result<(), DbError> {
    let episodes: Vec<Episode> = items.iter().filter_map(episode).collect();
    let raises = db.inbox_raises(repo, &episodes)?;
    let mut raises = raises.into_iter();
    for item in items.iter_mut() {
        item.raised = 0;
        if episode(item).is_some() {
            item.raised = raises.next().unwrap_or(0);
        }
    }
    Ok(())
}

fn number(board: &mut RepoBoard, episodes: &[Episode], raises: &[u64]) {
    for item in board.items.values_mut() {
        item.raised = 0;
    }
    for (episode, raise) in episodes.iter().zip(raises) {
        if let Some(item) = board.items.get_mut(&episode.key) {
            item.raised = *raise;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;

    const REPO: &str = "a3dda866c7e8f343";

    fn proposal(
        change: &str,
        commit: &str,
        bucket: v1::InboxBucket,
        reason: v1::InboxReason,
    ) -> v1::InboxItem {
        v1::InboxItem {
            key: format!("proposal:{REPO}:{change}"),
            repo_id: REPO.to_owned(),
            reason: reason.into(),
            commit_id: commit.to_owned(),
            record: Some(v1::inbox_item::Record::Proposal(v1::ProposalEntry {
                // Judged: an automatic status is in.
                automatic: Some(v1::AutomaticStatus::default()),
                ..v1::ProposalEntry::default()
            })),
            bucket: bucket.into(),
            ..v1::InboxItem::default()
        }
    }

    fn signoff(change: &str, commit: &str) -> v1::InboxItem {
        proposal(
            change,
            commit,
            v1::InboxBucket::NeedsYouNow,
            v1::InboxReason::Signoff,
        )
    }

    fn returned(change: &str, commit: &str) -> v1::InboxItem {
        proposal(
            change,
            commit,
            v1::InboxBucket::NeedsYouNow,
            v1::InboxReason::Returned,
        )
    }

    fn in_review(change: &str, commit: &str) -> v1::InboxItem {
        proposal(
            change,
            commit,
            v1::InboxBucket::Watching,
            v1::InboxReason::InReview,
        )
    }

    fn board(items: &[v1::InboxItem]) -> RepoBoard {
        let count = |bucket: v1::InboxBucket| {
            u32::try_from(items.iter().filter(|item| item.bucket() == bucket).count()).unwrap()
        };
        RepoBoard {
            rev: 1,
            items: items
                .iter()
                .map(|item| (item.key.clone(), item.clone()))
                .collect::<BTreeMap<_, _>>(),
            needs_you_now_total: count(v1::InboxBucket::NeedsYouNow),
            watching_total: count(v1::InboxBucket::Watching),
            error: None,
            subjects: Arc::default(),
            restale_at: None,
            read: true,
            raised_through: 0,
        }
    }

    fn settle_items(db: &Db, items: &[v1::InboxItem]) -> (BTreeMap<String, u64>, u64) {
        let repo: RepoId = REPO.parse().unwrap();
        let mut board = board(items);
        settle(db, repo, &mut board, None).unwrap();
        let raised = board
            .items
            .values()
            .map(|item| (item.key.rsplit(':').next().unwrap().to_owned(), item.raised))
            .collect();
        (raised, board.raised_through)
    }

    #[test]
    fn an_episode_keeps_its_raise_across_reads() {
        let db = Db::open_in_memory().unwrap();
        let (first, through) = settle_items(&db, &[signoff("a", "c1"), signoff("b", "c1")]);
        assert!(first["a"] > 0 && first["b"] > first["a"], "{first:?}");
        assert_eq!(through, first["b"]);
        let (again, through_again) = settle_items(&db, &[signoff("a", "c1"), signoff("b", "c1")]);
        assert_eq!(again, first, "the same read changes nothing");
        assert_eq!(through_again, through);
    }

    #[test]
    fn a_new_commit_or_ask_raises_again_and_a_restarts_reading_does_not() {
        let db = Db::open_in_memory().unwrap();
        let (first, _) = settle_items(&db, &[signoff("a", "c1")]);
        let (revised, _) = settle_items(&db, &[signoff("a", "c2")]);
        assert!(
            revised["a"] > first["a"],
            "a new revision is news: {revised:?}"
        );
        // After a restart, until the service judges it, a proposal reads from
        // its lifecycle (here returned): the commit's number stands.
        let mut restarted = returned("a", "c2");
        restarted.record = Some(v1::inbox_item::Record::Proposal(
            v1::ProposalEntry::default(),
        ));
        let (reading, _) = settle_items(&db, &[restarted]);
        assert_eq!(reading["a"], revised["a"], "{reading:?}");
        let (judged, _) = settle_items(&db, &[signoff("a", "c2")]);
        assert_eq!(judged["a"], revised["a"], "{judged:?}");
        // A judged new ask on the same commit is news: the landing after a
        // sign-off came back (review R1.F4, R4.F1).
        let (back, _) = settle_items(&db, &[returned("a", "c2")]);
        assert!(back["a"] > revised["a"], "{back:?}");
        let (signoff_again, _) = settle_items(&db, &[signoff("a", "c2")]);
        assert!(signoff_again["a"] > back["a"], "{signoff_again:?}");
    }

    fn unjudged(mut item: v1::InboxItem, status: Option<v1::AutomaticState>) -> v1::InboxItem {
        let automatic = status.map(|state| {
            let mut status = v1::AutomaticStatus::default();
            status.set_state(state);
            status
        });
        item.record = Some(v1::inbox_item::Record::Proposal(v1::ProposalEntry {
            automatic,
            ..v1::ProposalEntry::default()
        }));
        item
    }

    // R3.39 review R5.F1: a proposal left `Pending` past the first pass is
    // as unjudged as one with no status.
    #[test]
    fn a_pending_reading_keeps_the_commits_number() {
        let db = Db::open_in_memory().unwrap();
        let (judged, _) = settle_items(&db, &[signoff("a", "c1")]);
        let pending = unjudged(returned("a", "c1"), Some(v1::AutomaticState::Pending));
        let (reading, _) = settle_items(&db, &[pending]);
        assert_eq!(reading["a"], judged["a"]);
        let (again, _) = settle_items(&db, &[signoff("a", "c1")]);
        assert_eq!(again["a"], judged["a"]);
    }

    // R3.39 review R5.F2, R6.F1, R7.F1, R7.F2: a provisional reading with no
    // number for its commit numbers nothing; its first judgment does, once.
    #[test]
    fn a_provisional_reading_with_no_number_waits_for_its_judgment() {
        let db = Db::open_in_memory().unwrap();
        let (reading, through) = settle_items(&db, &[unjudged(returned("a", "c1"), None)]);
        assert_eq!(
            (reading["a"], through),
            (0, 0),
            "nothing announced at a restart's reading"
        );
        settle_items(
            &db,
            &[unjudged(
                in_review("a", "c1"),
                Some(v1::AutomaticState::Queued),
            )],
        );
        let (asked, _) = settle_items(&db, &[signoff("a", "c1")]);
        assert!(asked["a"] > 0, "the judged ask is news once");
        let (back, _) = settle_items(&db, &[returned("a", "c1")]);
        assert!(back["a"] > asked["a"]);
        let (again, _) = settle_items(&db, &[unjudged(returned("a", "c1"), None)]);
        assert_eq!(again["a"], back["a"], "a later restart's reading keeps it");
    }

    #[test]
    fn watching_keeps_the_episode_and_leaving_ends_it() {
        let db = Db::open_in_memory().unwrap();
        let (first, _) = settle_items(&db, &[signoff("a", "c1")]);
        let (judging, _) = settle_items(&db, &[in_review("a", "c1")]);
        assert_eq!(judging["a"], 0, "watching items have no raise");
        let (judged, _) = settle_items(&db, &[signoff("a", "c1")]);
        assert_eq!(judged["a"], first["a"], "back to the same episode: no news");
        settle_items(&db, &[]);
        let (asked_again, _) = settle_items(&db, &[signoff("a", "c1")]);
        assert!(
            asked_again["a"] > first["a"],
            "out of the inbox and back is news"
        );
    }

    #[test]
    fn a_cut_bucket_forgets_nothing() {
        let db = Db::open_in_memory().unwrap();
        let repo: RepoId = REPO.parse().unwrap();
        let (first, _) = settle_items(&db, &[signoff("a", "c1")]);
        let mut cut = board(&[]);
        cut.watching_total = 201;
        settle(&db, repo, &mut cut, None).unwrap();
        let (later, _) = settle_items(&db, &[signoff("a", "c1")]);
        assert_eq!(
            later["a"], first["a"],
            "a key past the cap may still be open"
        );
    }

    #[test]
    fn lookup_numbers_only_what_a_reader_settled() {
        let db = Db::open_in_memory().unwrap();
        let repo: RepoId = REPO.parse().unwrap();
        let (first, _) = settle_items(&db, &[signoff("a", "c1")]);
        let mut items = vec![signoff("a", "c1"), signoff("b", "c1"), signoff("a", "c2")];
        lookup(&db, repo, &mut items).unwrap();
        let raised: Vec<u64> = items.iter().map(|item| item.raised).collect();
        assert_eq!(
            raised,
            [first["a"], 0, 0],
            "an unsettled or moved episode reads 0"
        );
        let (after, _) = settle_items(&db, &[signoff("a", "c1")]);
        assert_eq!(after, first, "lookup wrote nothing");
    }

    // R3.39 review R1.F3: a key that fell past the cap and was answered there
    // is pruned once a whole read shows it gone, so asked again it is news.
    #[test]
    fn a_whole_read_after_a_cut_one_prunes() {
        let db = Db::open_in_memory().unwrap();
        let repo: RepoId = REPO.parse().unwrap();
        let mut first = board(&[signoff("a", "c1"), signoff("x", "c1")]);
        settle(&db, repo, &mut first, None).unwrap();
        let x = first.items[&format!("proposal:{REPO}:x")].raised;
        let mut cut = board(&[signoff("a", "c1")]);
        cut.needs_you_now_total = 2;
        settle(&db, repo, &mut cut, Some(&first)).unwrap();
        let mut whole = board(&[signoff("a", "c1")]);
        settle(&db, repo, &mut whole, Some(&cut)).unwrap();
        let mut again = board(&[signoff("a", "c1"), signoff("x", "c1")]);
        settle(&db, repo, &mut again, Some(&whole)).unwrap();
        assert!(again.items[&format!("proposal:{REPO}:x")].raised > x);
    }

    #[test]
    fn a_read_like_the_last_takes_its_numbers() {
        let db = Db::open_in_memory().unwrap();
        let repo: RepoId = REPO.parse().unwrap();
        let mut last = board(&[signoff("a", "c1"), in_review("b", "c1")]);
        settle(&db, repo, &mut last, None).unwrap();
        let mut next = board(&[signoff("a", "c1"), in_review("b", "c1")]);
        db.with(|conn| conn.execute("DELETE FROM inbox_raises", []))
            .unwrap();
        settle(&db, repo, &mut next, Some(&last)).unwrap();
        assert_eq!(next.items, last.items);
        assert_eq!(next.raised_through, last.raised_through);
    }
}
