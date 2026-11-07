//! A workspace's provenance session (R3.04): the identity and actor its
//! records are bound to, fixed when it starts, and the context a client
//! declared last.
//!
//! Which identity a workspace's session binds, by R3.03's rules:
//!
//! | Created through | Identity | When the rules refuse |
//! | --- | --- | --- |
//! | The socket, none named | the default `person` identity | no default: records unbound (`no_default_identity`) |
//! | The socket, one named | that `person` identity | the creation fails with R3.03's reason |
//! | A remote token linked to X | X; naming another is refused | the creation fails |
//! | A remote token with no link | none | records unbound (`token_not_linked`) |
//! | Older than R3.04 | the token's link, else the default | records unbound |
//!
//! A request that names nothing is never refused because of provenance.

use reef_api::v1;
use reef_evidence::session::Actor;

use super::{Held, Provenance, ProvenanceError, audit};
use crate::db::{DbError, DeclaredColumns, IdentityRow, ProvenanceSessionRow};
use crate::identity::issue::{Channel, Refusal, actor_of, check_rules};
use crate::identity::{IdentityError, new_id};
use crate::model::{RepoId, Timestamp, WorkspaceId, WorkspaceState};

/// Unbound because the daemon has no usable default identity.
pub const NO_DEFAULT_IDENTITY: &str = "no_default_identity";
/// Unbound because the creating remote token is linked to no identity.
pub const TOKEN_NOT_LINKED: &str = "token_not_linked";
/// Unbound because the creating remote token is revoked or expired.
pub const TOKEN_REVOKED: &str = "token_revoked";
/// Unbound because the session's identity is revoked.
pub const IDENTITY_REVOKED: &str = "identity_revoked";

/// The session's agent label: the request's `reef-agent`, else the
/// workspace's owner, whichever first is one a certificate can carry
/// (visible ASCII and spaces, 1 to 256 bytes); none when neither is.
fn agent_label(agent: Option<&str>, owner: &str) -> Option<String> {
    [agent, Some(owner)]
        .into_iter()
        .flatten()
        .find(|label| reef_evidence::session::agent_problem(label).is_none())
        .map(str::to_owned)
}

/// A change to a declared field: `None` keeps it, `Some(None)` clears it,
/// `Some(Some(value))` sets it.
type Change = Option<Option<String>>;

/// A validated declaration: what to change of the context.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    /// The model.
    pub model: Change,
    /// The harness.
    pub harness: Change,
    /// The task.
    pub task: Change,
    /// The instruction's SHA-256.
    pub instruction_sha256: Change,
    /// The client's session.
    pub client_session: Change,
}

impl Patch {
    /// Whether it changes nothing.
    pub fn is_empty(&self) -> bool {
        self.changed().is_empty()
    }

    /// The fields it changes, as the API names them.
    pub fn changed(&self) -> Vec<&'static str> {
        [
            ("model", &self.model),
            ("harness", &self.harness),
            ("task", &self.task),
            ("instruction_sha256", &self.instruction_sha256),
            ("client_session", &self.client_session),
        ]
        .into_iter()
        .filter(|(_, change)| change.is_some())
        .map(|(field, _)| field)
        .collect()
    }

    /// `declared` with this patch applied.
    pub fn apply(&self, declared: &DeclaredColumns) -> DeclaredColumns {
        let pick = |change: &Change, old: &Option<String>| match change {
            None => old.clone(),
            Some(value) => value.clone(),
        };
        DeclaredColumns {
            model: pick(&self.model, &declared.model),
            harness: pick(&self.harness, &declared.harness),
            task: pick(&self.task, &declared.task),
            instruction_sha256: pick(&self.instruction_sha256, &declared.instruction_sha256),
            client_session: pick(&self.client_session, &declared.client_session),
        }
    }
}

/// `declaration`'s context fields, checked: each set one non-empty and in
/// its bounds, or empty to clear. With `fixed`, a declaration naming an
/// identity or actor is refused: they are fixed for the session.
pub fn patch_of(
    declaration: &v1::ProvenanceDeclaration,
    fixed: bool,
) -> Result<Patch, ProvenanceError> {
    use reef_evidence::provenance::{instruction_problem, label_problem, task_problem};
    if fixed && !declaration.identity.is_empty() {
        return Err(ProvenanceError::Invalid {
            field: "identity",
            message: "a session's identity is fixed when its workspace is created".into(),
        });
    }
    if fixed && declaration.actor != 0 {
        return Err(ProvenanceError::Invalid {
            field: "actor",
            message: "a session's actor is fixed when its workspace is created".into(),
        });
    }
    let check = |field: &'static str,
                 value: &Option<String>,
                 problem: fn(&str) -> Option<String>|
     -> Result<Change, ProvenanceError> {
        match value.as_deref() {
            None => Ok(None),
            Some("") => Ok(Some(None)),
            Some(text) => match problem(text) {
                Some(message) => Err(ProvenanceError::Invalid { field, message }),
                None => Ok(Some(Some(text.to_owned()))),
            },
        }
    };
    Ok(Patch {
        model: check("model", &declaration.model, label_problem)?,
        harness: check("harness", &declaration.harness, label_problem)?,
        task: check("task", &declaration.task, task_problem)?,
        instruction_sha256: check(
            "instruction_sha256",
            &declaration.instruction_sha256,
            instruction_problem,
        )?,
        client_session: check("client_session", &declaration.client_session, label_problem)?,
    })
}

/// A session to start with a workspace, decided before the workspace is
/// created so that a refusal creates nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The identity its keys are certified for; `None` when none can be.
    pub identity_id: Option<String>,
    /// Its actor.
    pub actor: Actor,
    /// Why it is unbound, if it is.
    pub problem: Option<&'static str>,
    /// `socket`, or the creating token's id.
    pub channel: String,
    /// The initial context.
    pub patch: Patch,
}

/// The workspace a session is for.
#[derive(Clone, Copy, Debug)]
pub struct For<'a> {
    /// Its repository.
    pub repo_id: RepoId,
    /// Its id.
    pub workspace_id: WorkspaceId,
    /// Its owner: the agent label when the request names no `reef-agent`.
    pub owner: &'a str,
    /// The remote token that created it.
    pub remote_token_id: Option<&'a str>,
}

impl Provenance {
    /// The session a workspace created through `channel` with
    /// `declaration` would start; `None` when provenance is disabled. A
    /// named identity or actor the rules refuse is an error; naming none is
    /// never refused.
    pub fn plan(
        &self,
        channel: &Channel,
        declaration: Option<&v1::ProvenanceDeclaration>,
    ) -> Result<Option<Plan>, ProvenanceError> {
        let empty = v1::ProvenanceDeclaration::default();
        let declaration = declaration.unwrap_or(&empty);
        if !self.enabled() {
            if declaration != &empty {
                return Err(ProvenanceError::Disabled);
            }
            return Ok(None);
        }
        let patch = patch_of(declaration, false)?;
        let actor = if declaration.actor == 0 {
            Actor::Agent
        } else {
            actor_of(declaration.actor).map_err(|_| ProvenanceError::Invalid {
                field: "actor",
                message: "declare the session's actor: human, agent or executor".into(),
            })?
        };
        let named = declaration.identity.trim();
        let refused = |rule| ProvenanceError::Identity(IdentityError::Refused { rule });
        let (identity, problem) = match channel {
            // Naming nothing is never refused: a default revoked behind the
            // configuration's back records unbound.
            Channel::Socket if named.is_empty() => match self.identities.default_id() {
                Some(id) => match self.db.identity(id)? {
                    Some(row) if row.revocation.is_none() => (Some(row), None),
                    Some(_) => (None, Some(IDENTITY_REVOKED)),
                    None => (None, Some(NO_DEFAULT_IDENTITY)),
                },
                None => (None, Some(NO_DEFAULT_IDENTITY)),
            },
            Channel::Socket => (Some(self.identities.find(named)?), None),
            Channel::Remote { linked: None, .. } => {
                if actor == Actor::Human {
                    return Err(refused(Refusal::HumanOverRemote));
                }
                if !named.is_empty() {
                    return Err(refused(Refusal::TokenNotLinked));
                }
                (None, Some(TOKEN_NOT_LINKED))
            }
            Channel::Remote {
                linked: Some(id), ..
            } => {
                let linked = self.db.identity(id)?.ok_or_else(|| {
                    ProvenanceError::Identity(IdentityError::NotFound { name: id.clone() })
                })?;
                if !named.is_empty() && named != linked.name && named != linked.id {
                    return Err(refused(Refusal::TokenIdentityMismatch));
                }
                (Some(linked), None)
            }
        };
        if let Some(identity) = &identity {
            if let Some(revocation) = &identity.revocation {
                return Err(ProvenanceError::Identity(IdentityError::Revoked {
                    name: identity.name.clone(),
                    revoked_at: revocation.revoked_at,
                }));
            }
            check_rules(channel, identity, actor).map_err(refused)?;
        }
        Ok(Some(Plan {
            identity_id: identity.map(|identity: IdentityRow| identity.id),
            actor,
            problem,
            channel: channel.name().to_owned(),
            patch,
        }))
    }

    /// Starts workspace `at`'s session by `plan`, for the creating request's
    /// `agent` (else the owner). When a session started already (a record
    /// raced the creation), the plan's context is declared on it instead.
    pub fn begin(
        &self,
        plan: &Plan,
        at: For<'_>,
        agent: Option<&str>,
    ) -> Result<ProvenanceSessionRow, ProvenanceError> {
        let held = self.held(at.workspace_id);
        let mut held = held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let workspace = at.workspace_id.as_hex();
        let live = self.db.live_provenance_session(&workspace)?;
        if let Some(live) = &live
            && live.identity_id == plan.identity_id
            && live.actor == plan.actor.as_str()
            && live.channel == plan.channel
        {
            return self.declare_locked(&mut held, live.clone(), &plan.patch, agent);
        }
        let now = Timestamp::now();
        let row = ProvenanceSessionRow {
            session_id: self.unused_session_id()?,
            repo_id: at.repo_id,
            workspace_id: workspace,
            identity_id: plan.identity_id.clone(),
            actor: plan.actor.as_str().to_owned(),
            agent: agent_label(agent, at.owner),
            channel: plan.channel.clone(),
            declared: plan.patch.apply(&DeclaredColumns::default()),
            declared_at: (!plan.patch.is_empty()).then_some(now),
            declared_by_agent: (!plan.patch.is_empty())
                .then(|| agent.map(str::to_owned))
                .flatten(),
            next_sequence: 1,
            missing_records: 0,
            problem: plan.problem.map(str::to_owned),
            started_at: now,
            ended_at: None,
        };
        match &live {
            // A record raced the creation and started a session by the
            // rules with nothing named: the creation's binding replaces it,
            // and the records it signed stay with it.
            Some(raced) => {
                self.db
                    .restart_provenance_session(&raced.session_id, now, &row)?;
                audit::restarted(&raced.session_id, &row, at.workspace_id);
            }
            None => self.db.insert_provenance_session(&row)?,
        }
        audit::started(&row);
        *held = Held::default();
        Ok(row)
    }

    /// Workspace `at`'s live session, started now with the rules' identity
    /// for its channel when it has none (a workspace older than R3.04, or
    /// one whose session was lost). Holds `held`'s lock.
    pub(crate) fn live_or_start(
        &self,
        held: &mut Held,
        at: For<'_>,
    ) -> Result<ProvenanceSessionRow, ProvenanceError> {
        let workspace = at.workspace_id.as_hex();
        if let Some(live) = self.db.live_provenance_session(&workspace)? {
            return Ok(live);
        }
        // A workspace that has ended gets no new session: a record or a
        // declaration that raced its release is dropped.
        let state = self.db.workspace(at.workspace_id)?.state;
        if !matches!(state, WorkspaceState::Active | WorkspaceState::Releasing) {
            return Err(ProvenanceError::Ended(at.workspace_id));
        }
        let channel = match at.remote_token_id {
            None => Channel::Socket,
            Some(token_id) => {
                let linked = self
                    .db
                    .remote_tokens()?
                    .into_iter()
                    .find(|row| row.id == token_id)
                    .and_then(|row| row.identity_id);
                Channel::Remote {
                    token_id: token_id.to_owned(),
                    token_name: String::new(),
                    linked,
                }
            }
        };
        // The rules with nothing named never refuse but for a revoked
        // identity: that one records unbound.
        let plan = match self.plan(&channel, None) {
            Ok(Some(plan)) => plan,
            Ok(None) => return Err(ProvenanceError::Disabled),
            Err(error @ (ProvenanceError::Db(_) | ProvenanceError::Task(_))) => return Err(error),
            Err(_) => Plan {
                identity_id: None,
                actor: Actor::Agent,
                problem: Some(IDENTITY_REVOKED),
                channel: channel.name().to_owned(),
                patch: Patch::default(),
            },
        };
        let now = Timestamp::now();
        let row = ProvenanceSessionRow {
            session_id: self.unused_session_id()?,
            repo_id: at.repo_id,
            workspace_id: workspace,
            identity_id: plan.identity_id,
            actor: plan.actor.as_str().to_owned(),
            agent: agent_label(None, at.owner),
            channel: plan.channel,
            declared: DeclaredColumns::default(),
            declared_at: None,
            declared_by_agent: None,
            next_sequence: 1,
            missing_records: 0,
            problem: plan.problem.map(str::to_owned),
            started_at: now,
            ended_at: None,
        };
        self.db.insert_provenance_session(&row)?;
        audit::started(&row);
        *held = Held::default();
        Ok(row)
    }

    /// Replaces fields of workspace `at`'s declared context by `patch`, for
    /// the request's `agent`. The caller snapshots first.
    pub fn declare(
        &self,
        at: For<'_>,
        patch: &Patch,
        agent: Option<&str>,
    ) -> Result<ProvenanceSessionRow, ProvenanceError> {
        if !self.enabled() {
            return Err(ProvenanceError::Disabled);
        }
        let held = self.held(at.workspace_id);
        let mut held = held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let live = self.live_or_start(&mut held, at)?;
        self.declare_locked(&mut held, live, patch, agent)
    }

    fn declare_locked(
        &self,
        _held: &mut Held,
        mut live: ProvenanceSessionRow,
        patch: &Patch,
        agent: Option<&str>,
    ) -> Result<ProvenanceSessionRow, ProvenanceError> {
        let now = Timestamp::now();
        live.declared = patch.apply(&live.declared);
        live.declared_at = Some(now);
        live.declared_by_agent = agent.map(str::to_owned);
        self.db
            .declare_provenance(&live.session_id, &live.declared, now, agent)?;
        audit::declared(&live, &patch.changed(), agent);
        Ok(live)
    }

    /// Ends workspace `id`'s session, after its final snapshot recorded,
    /// and drops its held key.
    pub fn end(&self, id: WorkspaceId) {
        {
            let held = self.held(id);
            let _held = held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = self
                .db
                .end_provenance_session(&id.as_hex(), Timestamp::now())
            {
                tracing::warn!(
                    workspace_id = %id,
                    error = %reef_lib::error::ErrorChain(&error),
                    "could not end the workspace's provenance session"
                );
            }
        }
        self.forget(id);
    }

    /// A session id no session, certified or not, has.
    pub(crate) fn unused_session_id(&self) -> Result<String, DbError> {
        loop {
            let id = new_id("ses_");
            if !self.db.session_id_taken(&id)? && self.db.provenance_session(&id)?.is_none() {
                return Ok(id);
            }
        }
    }

    /// `row` as the API's `ProvenanceSession`, with its record count.
    pub fn session_message(
        &self,
        row: &ProvenanceSessionRow,
        records: u64,
    ) -> Result<v1::ProvenanceSession, DbError> {
        let identity = match &row.identity_id {
            Some(id) => self.db.identity(id)?,
            None => None,
        };
        let mut message = v1::ProvenanceSession {
            session_id: row.session_id.clone(),
            identity: identity
                .as_ref()
                .map(|identity| identity.name.clone())
                .unwrap_or_default(),
            agent: row.agent.clone().unwrap_or_default(),
            context: Some(context_message(&row.declared, row.declared_at)),
            started_at: Some(crate::service::convert::timestamp(row.started_at)),
            ended_at: row.ended_at.map(crate::service::convert::timestamp),
            records,
            missing_records: u64::try_from(row.missing_records).unwrap_or(0),
            problem: row.problem.clone().unwrap_or_default(),
            ..v1::ProvenanceSession::default()
        };
        message.set_identity_kind(crate::identity::read::kind_message(
            identity
                .as_ref()
                .and_then(|identity| reef_evidence::session::IdentityKind::parse(&identity.kind)),
        ));
        message.set_actor(crate::identity::read::actor_message(Actor::parse(
            &row.actor,
        )));
        Ok(message)
    }
}

/// A declared context in the API.
pub fn context_message(
    declared: &DeclaredColumns,
    declared_at: Option<Timestamp>,
) -> v1::ProvenanceContext {
    v1::ProvenanceContext {
        model: declared.model.clone().unwrap_or_default(),
        harness: declared.harness.clone().unwrap_or_default(),
        task: declared.task.clone().unwrap_or_default(),
        instruction_sha256: declared.instruction_sha256.clone().unwrap_or_default(),
        client_session: declared.client_session.clone().unwrap_or_default(),
        declared_at: declared_at.map(crate::service::convert::timestamp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches() {
        let declaration = v1::ProvenanceDeclaration {
            model: Some("m/1".into()),
            task: Some(String::new()),
            ..v1::ProvenanceDeclaration::default()
        };
        let patch = patch_of(&declaration, true).unwrap();
        assert_eq!(patch.changed(), ["model", "task"]);
        let old = DeclaredColumns {
            task: Some("T-1".into()),
            harness: Some("h/1".into()),
            ..DeclaredColumns::default()
        };
        let new = patch.apply(&old);
        assert_eq!(new.model.as_deref(), Some("m/1"));
        assert_eq!(new.task, None);
        assert_eq!(new.harness.as_deref(), Some("h/1"));
        let bad = v1::ProvenanceDeclaration {
            instruction_sha256: Some("not hex".into()),
            ..v1::ProvenanceDeclaration::default()
        };
        assert!(matches!(
            patch_of(&bad, true),
            Err(ProvenanceError::Invalid {
                field: "instruction_sha256",
                ..
            })
        ));
        let fixed = v1::ProvenanceDeclaration {
            identity: "alice".into(),
            ..v1::ProvenanceDeclaration::default()
        };
        assert!(patch_of(&fixed, false).is_ok());
        assert!(matches!(
            patch_of(&fixed, true),
            Err(ProvenanceError::Invalid {
                field: "identity",
                ..
            })
        ));
    }
}
