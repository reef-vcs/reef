//! Which entries a subscription follows. An entry's filtered fields are
//! fixed for its life, so whether it matches is decided once.

use std::collections::HashSet;

use super::hub::Change;
use crate::model::{RepoId, WorkspaceId};

/// A `SubscribePresenceRequest`'s fields, resolved: `None` is no
/// constraint; an entry matches a field holding any of its values, and
/// must match every field.
#[derive(Clone, Debug, Default)]
pub struct PresenceFilter {
    /// Repositories.
    pub repos: Option<HashSet<RepoId>>,
    /// Workspaces.
    pub workspaces: Option<HashSet<WorkspaceId>>,
    /// Agents, exactly.
    pub agents: Option<HashSet<String>>,
    /// `PresenceKind` values.
    pub kinds: Option<HashSet<i32>>,
}

impl PresenceFilter {
    /// Whether `change`'s entry is one this filter follows.
    pub fn matches(&self, change: &Change) -> bool {
        self.repos
            .as_ref()
            .is_none_or(|repos| change.repo.is_some_and(|repo| repos.contains(&repo)))
            && self.workspaces.as_ref().is_none_or(|workspaces| {
                change
                    .workspace
                    .is_some_and(|workspace| workspaces.contains(&workspace))
            })
            && self
                .agents
                .as_ref()
                .is_none_or(|agents| agents.contains(&change.presence.agent))
            && self
                .kinds
                .as_ref()
                .is_none_or(|kinds| kinds.contains(&change.presence.kind))
    }
}

#[cfg(test)]
mod tests {
    use reef_api::v1::{self, PresenceKind};

    use super::*;
    use crate::model::PresenceId;

    fn change(repo: Option<RepoId>, agent: &str, kind: PresenceKind) -> Change {
        Change {
            id: PresenceId::generate(),
            seq: 1,
            repo,
            workspace: None,
            presence: v1::Presence {
                agent: agent.to_owned(),
                kind: kind.into(),
                ..v1::Presence::default()
            },
            end: None,
        }
    }

    #[test]
    fn fields_are_anded_and_values_ored() {
        let (a, b) = (RepoId::generate(), RepoId::generate());
        let filter = PresenceFilter {
            repos: Some(HashSet::from([a, b])),
            kinds: Some(HashSet::from([PresenceKind::Activity.into()])),
            ..PresenceFilter::default()
        };
        assert!(filter.matches(&change(Some(a), "x", PresenceKind::Activity)));
        assert!(filter.matches(&change(Some(b), "x", PresenceKind::Activity)));
        assert!(!filter.matches(&change(Some(a), "x", PresenceKind::Editing)));
        assert!(
            !filter.matches(&change(None, "x", PresenceKind::Activity)),
            "no repository never matches a repository filter"
        );
        assert!(PresenceFilter::default().matches(&change(None, "x", PresenceKind::Editing)));
    }
}
