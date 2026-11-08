//! [`OpMatcher`]: a request's `OperationFilter`, resolved, as
//! `ListOperations` (R2.02) and `SubscribeOperations` (R2.09) apply it:
//! values joined by OR within a field, fields by AND.

use std::collections::HashSet;

use reef_api::v1;
use reef_lib::jj::effects::OpEffects;
use reef_lib::jj::oplog::{self, ATTR_AGENT, OpClass, OpEntry};
use tonic::Status;

use super::convert::{self, known};
use super::operations::{InvalidChangeFilter, change_prefix};
use crate::daemon::AppState;
use crate::error::status;

/// An `OperationFilter`, resolved.
#[derive(Debug, Default)]
pub(crate) struct OpMatcher {
    workspaces: HashSet<String>,
    changes: Vec<String>,
    agents: HashSet<String>,
    kinds: HashSet<i32>,
}

impl OpMatcher {
    /// `filter` resolved: workspaces as `GetWorkspace` resolves them in
    /// `repo` (empty for any repository), in any state; change ids as hex
    /// or reverse-hex prefixes; kinds checked.
    pub(crate) fn resolve(
        state: &AppState,
        repo: &str,
        filter: v1::OperationFilter,
    ) -> Result<OpMatcher, Status> {
        let mut matcher = OpMatcher {
            agents: filter.agents.into_iter().collect(),
            ..OpMatcher::default()
        };
        for workspace in &filter.workspaces {
            let record = state
                .manager
                .resolve_workspace(repo, workspace)
                .map_err(status)?;
            matcher.workspaces.insert(record.id.as_hex());
        }
        for change in filter.changes {
            let prefix =
                change_prefix(&change).ok_or_else(|| status(InvalidChangeFilter { change }))?;
            matcher.changes.push(prefix);
        }
        for kind in filter.kinds {
            let kind: v1::OperationKind = known("filter.kinds", kind).map_err(status)?;
            matcher.kinds.insert(kind.into());
        }
        Ok(matcher)
    }

    /// Whether the filter names changes, so [`OpMatcher::touches`] needs
    /// each kept operation's effects.
    pub(crate) fn names_changes(&self) -> bool {
        !self.changes.is_empty()
    }

    /// Whether the filter keeps `entry` by kind, workspace and agent;
    /// `owner` gives a workspace's owner, the agent of an operation without
    /// `reef.agent`.
    pub(crate) fn keeps<'a>(
        &self,
        entry: &OpEntry,
        owner: impl Fn(&str) -> Option<&'a str>,
    ) -> bool {
        self.keeps_classified(entry, &oplog::classify(entry), owner)
    }

    /// [`OpMatcher::keeps`] with `entry`'s classification already made:
    /// R2.22's broker classifies an operation once for every session.
    pub(crate) fn keeps_classified<'a>(
        &self,
        entry: &OpEntry,
        class: &OpClass,
        owner: impl Fn(&str) -> Option<&'a str>,
    ) -> bool {
        // An undo of a transition (kind UNDO) carries transitions too, and
        // `transition` keeps it: whoever waits for a proposal's fate sees
        // it undone (R3.09).
        let transitions = self
            .kinds
            .contains(&i32::from(v1::OperationKind::Transition))
            && entry.attributes.contains_key(oplog::ATTR_LIFECYCLE);
        if !self.kinds.is_empty()
            && !transitions
            && !self
                .kinds
                .contains(&i32::from(convert::operation_kind(class.kind)))
        {
            return false;
        }
        let workspace = class.workspace_id.as_deref();
        if !self.workspaces.is_empty() && !workspace.is_some_and(|id| self.workspaces.contains(id))
        {
            return false;
        }
        if !self.agents.is_empty() {
            let agent = entry
                .attributes
                .get(ATTR_AGENT)
                .map(String::as_str)
                .or_else(|| workspace.and_then(&owner));
            if !agent.is_some_and(|agent| self.agents.contains(agent)) {
                return false;
            }
        }
        true
    }

    /// Whether `entry`'s `effects`, or the lifecycle transitions it
    /// records (R3.09), touch a change the filter names; false without
    /// either.
    pub(crate) fn touches(&self, entry: &OpEntry, effects: Option<&OpEffects>) -> bool {
        if effects.is_some_and(|effects| self.changes.iter().any(|prefix| effects.touches(prefix)))
        {
            return true;
        }
        if !entry.attributes.contains_key(oplog::ATTR_LIFECYCLE) {
            return false;
        }
        reef_lib::jj::lifecycle::transitions_of(&entry.attributes)
            .iter()
            .any(|transition| {
                self.changes
                    .iter()
                    .any(|prefix| transition.change.starts_with(prefix.as_str()))
            })
    }
}
