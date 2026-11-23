//! What the shell posts (R3.39, ADR 0093): banners for needs-you-now items
//! that are news, and their retraction once handled.
//!
//! Which items are news is [`reef_client::inbox::News`], the rule `reef
//! inbox watch` uses too, kept on disk so neither an app nor a daemon
//! restart announces anything twice. A banner's identifier is the item's
//! key, so posting again replaces it: the cursor is saved after posting
//! ([`Notifier::commit`]), and a crash between the two can repeat a banner
//! but never lose one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reef_api::v1;
use reef_client::inbox::News;
use reef_client::web::WebPath;
use tokio::time::Instant;

/// More than this many news items in one frame post one summary instead.
pub const SUMMARY_OVER: usize = 3;
/// A posted item that leaves needs-you-now is taken back after this unless
/// it is back: a daemon restart judges proposals again, and one reads as
/// watching for a moment.
pub const RETRACT_AFTER: Duration = Duration::from_secs(30);
/// The summary banner's identifier.
pub const SUMMARY_ID: &str = "summary";

/// A banner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// The identifier: the item's key, or [`SUMMARY_ID`].
    pub id: String,
    /// The daemon's headline (never an agent's text, S-262).
    pub title: String,
    /// The repository's name.
    pub body: String,
    /// The page a click opens.
    pub open: WebPath,
}

/// What one step asks the platform to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Banners to post.
    pub post: Vec<Note>,
    /// Identifiers of banners to take back.
    pub retract: Vec<String>,
}

/// The notifier: the saved cursor, and this run's banners.
#[derive(Debug)]
pub struct Notifier {
    news: News,
    file: PathBuf,
    /// Whether `news` moved since it was last saved.
    unsaved: bool,
    /// Banners posted this run: each key, with its repository.
    posted: BTreeMap<String, String>,
    leaving: BTreeMap<String, Instant>,
    /// The keys the summary banner counted.
    summary: BTreeSet<String>,
    stream: Stream,
}

/// What the current stream has sent, to tell what left while the shell was
/// away: a new stream's listings carry no `left`.
#[derive(Debug, Default)]
struct Stream {
    /// Every key each repository's listing on this stream has shown so far.
    listing: BTreeMap<String, BTreeSet<String>>,
    seen: BTreeSet<String>,
    synced: bool,
}

impl Notifier {
    /// The cursor saved at `file`; none (a first run, or a file that does
    /// not read) starts every repository quietly at its listing.
    pub fn load(file: &Path) -> Notifier {
        let news = std::fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Notifier {
            news,
            file: file.to_owned(),
            unsaved: false,
            posted: BTreeMap::new(),
            leaving: BTreeMap::new(),
            summary: BTreeSet::new(),
            stream: Stream::default(),
        }
    }

    /// A new stream starts: its listings say what is still there.
    pub fn new_stream(&mut self) {
        self.stream = Stream::default();
    }

    /// The banners `frame` asks for, `names` naming repositories by id.
    /// The cursor moves here and is saved by [`Notifier::commit`], once the
    /// banners are posted.
    pub fn frame(
        &mut self,
        frame: &v1::WatchInboxResponse,
        names: &BTreeMap<String, String>,
        now: Instant,
    ) -> Step {
        let before = self.news.clone();
        self.news.epoch(&frame.raise_epoch);
        let mut news: Vec<(&v1::InboxItem, &str)> = Vec::new();
        for section in &frame.sections {
            for item in self.news.section(section) {
                news.push((item, &section.repo_id));
            }
            self.leave(section, now);
        }
        if frame.synced && !self.stream.synced {
            self.stream.synced = true;
            let unsent: Vec<String> = self
                .posted
                .iter()
                .filter(|(_, repo)| !self.stream.seen.contains(*repo))
                .map(|(key, _)| key.clone())
                .collect();
            for key in unsent {
                self.leaving.entry(key).or_insert(now + RETRACT_AFTER);
            }
        }
        let mut step = Step::default();
        let name = |repo: &str| names.get(repo).cloned().unwrap_or_else(|| short(repo));
        if news.len() > SUMMARY_OVER {
            step.post.push(Note {
                id: SUMMARY_ID.to_owned(),
                title: format!("{} things need you", news.len()),
                body: "Reef".to_owned(),
                open: WebPath::start(),
            });
            self.summary = news.iter().map(|(item, _)| item.key.clone()).collect();
        } else {
            for (item, repo) in &news {
                step.post.push(Note {
                    id: item.key.clone(),
                    title: item.headline.clone(),
                    body: name(repo),
                    open: WebPath::item(repo, &item.key).unwrap_or_else(WebPath::start),
                });
            }
        }
        for (item, repo) in news {
            self.posted.insert(item.key.clone(), repo.to_owned());
            self.leaving.remove(&item.key);
        }
        self.unsaved |= self.news != before;
        step
    }

    /// Saves the cursor, if it moved; call it after posting what
    /// [`Notifier::frame`] asked for.
    pub fn commit(&mut self) {
        if self.unsaved {
            self.save();
            self.unsaved = false;
        }
    }

    fn leave(&mut self, section: &v1::InboxSectionChange, now: Instant) {
        let posted = |key: &str| self.posted.contains_key(key);
        let mut gone: Vec<String> = section
            .left
            .iter()
            .filter(|key| posted(key))
            .cloned()
            .collect();
        self.stream.seen.insert(section.repo_id.clone());
        if section.listing() != v1::InboxListing::Unspecified {
            let listed = self
                .stream
                .listing
                .entry(section.repo_id.clone())
                .or_default();
            listed.extend(section.items.iter().map(|item| item.key.clone()));
            if section.listing() == v1::InboxListing::End {
                gone.extend(
                    self.posted
                        .iter()
                        .filter(|(key, repo)| **repo == section.repo_id && !listed.contains(*key))
                        .map(|(key, _)| key.clone()),
                );
            }
        }
        if section.removed {
            gone.extend(
                self.posted
                    .iter()
                    .filter(|(_, repo)| **repo == section.repo_id)
                    .map(|(key, _)| key.clone()),
            );
        }
        for item in &section.items {
            if !posted(&item.key) {
                continue;
            }
            if item.bucket() == v1::InboxBucket::NeedsYouNow {
                self.leaving.remove(&item.key);
            } else {
                gone.push(item.key.clone());
            }
        }
        for key in gone {
            self.leaving.entry(key).or_insert(now + RETRACT_AFTER);
        }
    }

    /// Takes back what left and stayed away.
    pub fn tick(&mut self, now: Instant) -> Step {
        let due: Vec<String> = self
            .leaving
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &due {
            self.leaving.remove(key);
            self.posted.remove(key);
        }
        let mut retract = due;
        if !self.summary.is_empty()
            && self
                .summary
                .iter()
                .all(|key| !self.posted.contains_key(key))
        {
            self.summary.clear();
            retract.push(SUMMARY_ID.to_owned());
        }
        Step {
            post: Vec::new(),
            retract,
        }
    }

    /// When [`Notifier::tick`] next has something to take back.
    pub fn deadline(&self) -> Option<Instant> {
        self.leaving.values().min().copied()
    }

    fn save(&self) {
        let Ok(bytes) = serde_json::to_vec(&self.news) else {
            return;
        };
        let temporary = self.file.with_extension("json.tmp");
        let saved = std::fs::write(&temporary, bytes)
            .and_then(|()| std::fs::rename(&temporary, &self.file));
        if let Err(error) = saved {
            tracing::warn!(%error, file = %self.file.display(), "could not save the notification cursor");
        }
    }
}

fn short(repo: &str) -> String {
    repo.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "a3dda866c7e8f343";

    fn help(id: &str, raised: u64) -> v1::InboxItem {
        v1::InboxItem {
            key: format!("help:{REPO}:{id}"),
            repo_id: REPO.to_owned(),
            headline: format!("Asks for help ({id})"),
            bucket: v1::InboxBucket::NeedsYouNow.into(),
            raised,
            ..v1::InboxItem::default()
        }
    }

    fn frame(sections: Vec<v1::InboxSectionChange>) -> v1::WatchInboxResponse {
        v1::WatchInboxResponse {
            sections,
            synced: true,
            raise_epoch: "epoch".to_owned(),
        }
    }

    fn section(
        listing: v1::InboxListing,
        items: Vec<v1::InboxItem>,
        left: Vec<String>,
        through: u64,
    ) -> v1::InboxSectionChange {
        v1::InboxSectionChange {
            repo_id: REPO.to_owned(),
            items,
            left,
            listing: listing.into(),
            raised_through: through,
            ..v1::InboxSectionChange::default()
        }
    }

    fn names() -> BTreeMap<String, String> {
        BTreeMap::from([(REPO.to_owned(), "reef".to_owned())])
    }

    use v1::InboxListing::{End, Unspecified};

    #[test]
    fn a_first_run_is_quiet_and_news_posts_once_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notices.json");
        let mut notifier = Notifier::load(&file);
        let now = Instant::now();
        let listed = frame(vec![section(End, vec![help("old", 1)], vec![], 1)]);
        assert_eq!(notifier.frame(&listed, &names(), now), Step::default());
        let asked = frame(vec![section(Unspecified, vec![help("new", 2)], vec![], 2)]);
        let step = notifier.frame(&asked, &names(), now);
        notifier.commit();
        assert_eq!(step.post.len(), 1);
        let note = &step.post[0];
        assert_eq!(note.id, format!("help:{REPO}:new"));
        assert_eq!(
            (note.title.as_str(), note.body.as_str()),
            ("Asks for help (new)", "reef")
        );
        assert_eq!(
            note.open.as_str(),
            format!("/repos/{REPO}/items/help:{REPO}:new")
        );

        let mut again = Notifier::load(&file);
        let relisted = frame(vec![section(
            End,
            vec![help("old", 1), help("new", 2)],
            vec![],
            2,
        )]);
        assert_eq!(again.frame(&relisted, &names(), now), Step::default());
        again.commit();
        let mut later = Notifier::load(&file);
        let away = frame(vec![section(End, vec![help("away", 3)], vec![], 3)]);
        assert_eq!(later.frame(&away, &names(), now).post.len(), 1);
    }

    #[test]
    fn a_burst_posts_one_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut notifier = Notifier::load(&dir.path().join("notices.json"));
        let now = Instant::now();
        notifier.frame(&frame(vec![section(End, vec![], vec![], 0)]), &names(), now);
        let burst: Vec<v1::InboxItem> = (1..=4).map(|n| help(&n.to_string(), n)).collect();
        let step = notifier.frame(
            &frame(vec![section(Unspecified, burst, vec![], 4)]),
            &names(),
            now,
        );
        assert_eq!(step.post.len(), 1);
        assert_eq!(step.post[0].id, SUMMARY_ID);
        assert_eq!(step.post[0].title, "4 things need you");
    }

    #[test]
    fn a_banner_is_taken_back_once_it_stays_away() {
        let dir = tempfile::tempdir().unwrap();
        let mut notifier = Notifier::load(&dir.path().join("notices.json"));
        let now = Instant::now();
        notifier.frame(&frame(vec![section(End, vec![], vec![], 0)]), &names(), now);
        let key = format!("help:{REPO}:a");
        notifier.frame(
            &frame(vec![section(Unspecified, vec![help("a", 1)], vec![], 1)]),
            &names(),
            now,
        );

        let mut watching = help("a", 0);
        watching.bucket = v1::InboxBucket::Watching.into();
        notifier.frame(
            &frame(vec![section(Unspecified, vec![watching], vec![], 1)]),
            &names(),
            now,
        );
        assert_eq!(notifier.deadline(), Some(now + RETRACT_AFTER));
        let back = notifier.frame(
            &frame(vec![section(Unspecified, vec![help("a", 1)], vec![], 1)]),
            &names(),
            now,
        );
        assert_eq!(back.post, [], "the same raise is not news");
        assert_eq!(notifier.deadline(), None);

        notifier.frame(
            &frame(vec![section(Unspecified, vec![], vec![key.clone()], 1)]),
            &names(),
            now,
        );
        assert_eq!(notifier.tick(now).retract, Vec::<String>::new());
        assert_eq!(notifier.tick(now + RETRACT_AFTER).retract, [key]);
    }

    #[test]
    fn another_database_starts_over_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let mut notifier = Notifier::load(&dir.path().join("notices.json"));
        let now = Instant::now();
        notifier.frame(&frame(vec![section(End, vec![], vec![], 5)]), &names(), now);
        let mut restored = frame(vec![section(End, vec![help("x", 2)], vec![], 2)]);
        restored.raise_epoch = "restored".to_owned();
        assert_eq!(notifier.frame(&restored, &names(), now).post, []);
    }

    // R3.39 review R1.F5: a new stream's listings carry no `left`; what they
    // lack left while the shell was away.
    #[test]
    fn a_reconnect_takes_back_what_left_while_away() {
        let dir = tempfile::tempdir().unwrap();
        let mut notifier = Notifier::load(&dir.path().join("notices.json"));
        let now = Instant::now();
        notifier.frame(&frame(vec![section(End, vec![], vec![], 0)]), &names(), now);
        notifier.frame(
            &frame(vec![section(Unspecified, vec![help("a", 1)], vec![], 1)]),
            &names(),
            now,
        );
        notifier.new_stream();
        notifier.frame(
            &frame(vec![section(End, vec![help("b", 1)], vec![], 1)]),
            &names(),
            now,
        );
        assert_eq!(
            notifier.tick(now + RETRACT_AFTER).retract,
            [format!("help:{REPO}:a")]
        );
    }

    #[test]
    fn a_repository_gone_while_away_takes_its_banners_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut notifier = Notifier::load(&dir.path().join("notices.json"));
        let now = Instant::now();
        notifier.frame(&frame(vec![section(End, vec![], vec![], 0)]), &names(), now);
        notifier.frame(
            &frame(vec![section(Unspecified, vec![help("a", 1)], vec![], 1)]),
            &names(),
            now,
        );
        notifier.new_stream();
        notifier.frame(&frame(vec![]), &names(), now);
        assert_eq!(
            notifier.tick(now + RETRACT_AFTER).retract,
            [format!("help:{REPO}:a")]
        );
    }

    #[test]
    fn the_summary_goes_once_everything_it_counted_has() {
        let dir = tempfile::tempdir().unwrap();
        let mut notifier = Notifier::load(&dir.path().join("notices.json"));
        let now = Instant::now();
        notifier.frame(&frame(vec![section(End, vec![], vec![], 0)]), &names(), now);
        let burst: Vec<v1::InboxItem> = (1..=4).map(|n| help(&n.to_string(), n)).collect();
        notifier.frame(
            &frame(vec![section(Unspecified, burst, vec![], 4)]),
            &names(),
            now,
        );
        let left: Vec<String> = (1..=4).map(|n| format!("help:{REPO}:{n}")).collect();
        notifier.frame(
            &frame(vec![section(Unspecified, vec![], left, 4)]),
            &names(),
            now,
        );
        assert!(
            notifier
                .tick(now + RETRACT_AFTER)
                .retract
                .contains(&SUMMARY_ID.to_owned())
        );
    }

    // R3.39 main-session review F1: a crash or a quit after the cursor moves
    // and before the banner is posted must not lose the banner.
    #[test]
    fn the_cursor_is_saved_only_after_the_banner_is_posted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notices.json");
        let mut notifier = Notifier::load(&file);
        let now = Instant::now();
        notifier.frame(&frame(vec![section(End, vec![], vec![], 1)]), &names(), now);
        notifier.commit();
        let asked = frame(vec![section(Unspecified, vec![help("new", 2)], vec![], 2)]);
        assert_eq!(notifier.frame(&asked, &names(), now).post.len(), 1);
        let relisted = frame(vec![section(End, vec![help("new", 2)], vec![], 2)]);
        let mut crashed = Notifier::load(&file);
        assert_eq!(
            crashed.frame(&relisted, &names(), now).post.len(),
            1,
            "a banner never posted is posted after the restart"
        );
        notifier.commit();
        let mut posted = Notifier::load(&file);
        assert_eq!(
            posted.frame(&relisted, &names(), now).post,
            [],
            "a posted banner is not posted again"
        );
    }
}
