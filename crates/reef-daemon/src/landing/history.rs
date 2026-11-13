//! The landed history projection's settings and trailers (R3.16, ADR 0075).
//!
//! A batch reads the destination policy's `[history]` table with its
//! `[landing]` table, at its base. When it says `message = "summary"`, each
//! member lands with a [`Summary`]: the trailer sets the table names, in
//! order, which `reef_lib::jj::land` appends to the change's own normalized
//! description after removing the trailers the projection owns. The facts
//! come from the landing's records: its id and batch, the change's
//! provenance agents, and the people whose sign-offs its evaluation
//! counted.

use reef_lib::jj::land::Summary;
use reef_policy::{Evaluation, HistorySettings, Message, RuleKind, Status, TrailerSet};
use serde::{Deserialize, Serialize};

use crate::db::{Db, DbError};
use crate::model::RepoId;

/// The most `Reef-Agent` trailers one change gets.
pub(crate) const MAX_AGENTS: usize = 5;
/// The most `Reviewed-by` trailers one change gets.
pub(crate) const MAX_REVIEWERS: usize = 10;

/// `landings.history`: the `[history]` table a landing landed under.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Recorded {
    /// `summary` or `keep`.
    pub(crate) message: String,
    /// The trailer sets written, in order.
    #[serde(default)]
    pub(crate) trailers: Vec<String>,
}

impl Recorded {
    /// The record of `settings`.
    pub(crate) fn of(settings: &HistorySettings) -> Recorded {
        Recorded {
            message: settings.message.as_str().to_owned(),
            trailers: settings
                .trailers
                .iter()
                .map(|set| set.as_str().to_owned())
                .collect(),
        }
    }

    /// As the column holds it.
    pub(crate) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The column read back; `None` when it does not parse.
    pub(crate) fn parse(json: &str) -> Option<Recorded> {
        serde_json::from_str(json).ok()
    }
}

/// What one member's trailers name.
pub(crate) struct Facts<'a> {
    /// The change, in hex.
    pub(crate) change: &'a str,
    /// Its landing.
    pub(crate) landing: &'a str,
    /// Its batch, when the batch lands more than one change.
    pub(crate) batch: Option<&'a str>,
    /// Its evaluation at landing (on its submitted tree).
    pub(crate) evaluation: &'a Evaluation,
}

/// The [`Summary`] a member of a landing gets under `settings`; `None`
/// under `message = "keep"`.
pub(crate) fn summary(
    db: &Db,
    repo: RepoId,
    settings: &HistorySettings,
    facts: &Facts<'_>,
) -> Result<Option<Summary>, DbError> {
    if settings.message != Message::Summary {
        return Ok(None);
    }
    let mut summary = Summary::default();
    for set in &settings.trailers {
        match set {
            TrailerSet::Change => summary
                .trailers
                .push(("Reef-Change".to_owned(), facts.change.to_owned())),
            TrailerSet::Landing => {
                summary
                    .trailers
                    .push(("Reef-Landing".to_owned(), facts.landing.to_owned()));
                if let Some(batch) = facts.batch {
                    summary
                        .trailers
                        .push(("Reef-Batch".to_owned(), batch.to_owned()));
                }
            }
            TrailerSet::Agents => {
                for agent in db.provenance_agents(repo, facts.change, MAX_AGENTS)? {
                    summary.trailers.push(("Reef-Agent".to_owned(), agent));
                }
            }
            TrailerSet::Signoffs => {
                summary.strip.push("Reviewed-by".to_owned());
                for reviewer in reviewers(db, facts.evaluation)? {
                    summary.trailers.push(("Reviewed-by".to_owned(), reviewer));
                }
            }
        }
    }
    Ok(Some(summary))
}

/// `Name <email>` (or the name alone) for each person whose sign-off a
/// satisfied `[[signoff]]` rule or `builtin:owners` counted in
/// `evaluation`, sorted, at most [`MAX_REVIEWERS`].
fn reviewers(db: &Db, evaluation: &Evaluation) -> Result<Vec<String>, DbError> {
    let mut names: Vec<&str> = evaluation
        .rules
        .iter()
        .filter(|rule| rule.status == Status::Satisfied)
        .filter(|rule| {
            rule.kind == RuleKind::Signoff
                || (rule.kind == RuleKind::Builtin && rule.name == "builtin:owners")
        })
        .filter_map(|rule| rule.signoff.as_ref())
        .flat_map(|signoff| signoff.signed_by.iter().map(String::as_str))
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut reviewers = Vec::new();
    for name in names {
        let Some(identity) = db.identity(name)? else {
            continue;
        };
        if identity.kind != "person" {
            continue;
        }
        let display = identity
            .display_name
            .filter(|display| !display.trim().is_empty())
            .unwrap_or(identity.name);
        reviewers.push(
            match identity.email.filter(|email| !email.trim().is_empty()) {
                Some(email) => format!("{display} <{email}>"),
                None => display,
            },
        );
    }
    reviewers.sort();
    reviewers.dedup();
    reviewers.truncate(MAX_REVIEWERS);
    Ok(reviewers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let settings = HistorySettings {
            message: Message::Summary,
            trailers: vec![TrailerSet::Change, TrailerSet::Signoffs],
        };
        let recorded = Recorded::of(&settings);
        assert_eq!(recorded.trailers, ["change", "signoffs"]);
        assert_eq!(Recorded::parse(&recorded.to_json()), Some(recorded));
        assert_eq!(Recorded::parse("nope"), None);
        let keep = Recorded::of(&HistorySettings::default());
        assert_eq!(keep.message, "keep");
        assert_eq!(keep.trailers, Vec::<String>::new());
    }
}
