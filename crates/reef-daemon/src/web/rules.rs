//! The web listener's allowlist of writes (R3.43, ADR 0085; R3.31, ADR
//! 0086): beside every `ACCESS_READ` method, step 8 of the gate passes the
//! methods of [`TABLE`], and refuses every other WRITE and every ADMIN
//! method with `WEB_ACCESS_DENIED`. The gate sees only encoded bytes, so
//! each row's rule is the handler's to check, with the request's
//! [`super::Caller`]; the rows are what a browser holding a session may
//! change. A call a row's rule refuses is `WEB_ACCESS_DENIED` too, naming
//! the rule ([`denied`]).
//!
//! `IssueSessionKey` is not here: a browser gets a key only through an
//! approval, bound to one sign-off. Publishing, fetching, pushing and
//! creating workspaces, which run the user's git credentials or tools,
//! never are.

use reef_api::v1::{
    CommentKind, CreateCommentRequest, LandChangeRequest, LifecycleState, TransitionActor,
    TransitionChangeRequest, UpdateInstructionRequest,
};

/// A row's rule, checked by its handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rule {
    /// A person identity; empty is the default person
    /// (`crate::identity::passkeys`).
    PersonIdentity,
    /// The registration challenge this web UI session started; an
    /// assertion by one of the identity's passkeys when it has one.
    OwnChallenge,
    /// An assertion by one of the passkey's identity's live passkeys.
    PasskeyAssertion,
    /// A person identity, and a statement whose subject is the revision's
    /// tree (`crate::identity::approvals`).
    ApprovalRequest,
    /// Any pending approval, the CLI's or a browser's; an approve needs a
    /// valid assertion by a live passkey of the approval's identity.
    ApprovalResolution,
    /// Evidence signed by a `user_verified` key, for the statement and the
    /// change its passkey approved (`crate::service::evidence`).
    PresenceEvidence,
    /// A decline, at the commit the page showed ([`check_decline`]).
    Decline,
    /// An instruction's confirm or cancel, as the socket's rules allow
    /// them; never a claimer's release, start or finish
    /// ([`check_confirm`], `crate::comments::update`).
    Confirm,
    /// A person's note, instruction or answer, waiting for its confirm
    /// ([`check_comment`]).
    Comment,
    /// A landing a person asks for, at the commit the page showed
    /// ([`check_land`]).
    Land,
}

impl Rule {
    /// The rule's name, in a refusal's metadata.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Rule::PersonIdentity => "person_identity",
            Rule::OwnChallenge => "own_challenge",
            Rule::PasskeyAssertion => "passkey_assertion",
            Rule::ApprovalRequest => "approval_request",
            Rule::ApprovalResolution => "approval_resolution",
            Rule::PresenceEvidence => "presence_evidence",
            Rule::Decline => "decline",
            Rule::Confirm => "confirm",
            Rule::Comment => "comment",
            Rule::Land => "land",
        }
    }
}

/// The writes the web listener passes, and their rules.
pub(crate) const TABLE: &[(&str, Rule)] = &[
    (
        "/reef.api.v1.IdentityService/StartPasskeyRegistration",
        Rule::PersonIdentity,
    ),
    (
        "/reef.api.v1.IdentityService/RegisterPasskey",
        Rule::OwnChallenge,
    ),
    (
        "/reef.api.v1.IdentityService/RemovePasskey",
        Rule::PasskeyAssertion,
    ),
    (
        "/reef.api.v1.IdentityService/RequestApproval",
        Rule::ApprovalRequest,
    ),
    (
        "/reef.api.v1.IdentityService/ResolveApproval",
        Rule::ApprovalResolution,
    ),
    (
        "/reef.api.v1.EvidenceService/AddEvidence",
        Rule::PresenceEvidence,
    ),
    ("/reef.api.v1.ChangeService/TransitionChange", Rule::Decline),
    ("/reef.api.v1.CommentService/CreateComment", Rule::Comment),
    (
        "/reef.api.v1.CommentService/UpdateInstruction",
        Rule::Confirm,
    ),
    ("/reef.api.v1.LandingService/LandChange", Rule::Land),
];

/// A web call `rule` refuses.
pub(crate) fn denied(method: &str, rule: Rule) -> super::WebError {
    super::WebError::RuleDenied {
        method: method.to_owned(),
        rule: rule.as_str(),
    }
}

/// Whether a web request's declared actor is a person's: `HUMAN`, or none,
/// which is a person's since a web call carries no agent name.
fn person(actor: TransitionActor) -> bool {
    matches!(actor, TransitionActor::Human | TransitionActor::Unspecified)
}

fn refuse(web: bool, refused: bool, method: &str, rule: Rule) -> Result<(), super::WebError> {
    if web && refused {
        return Err(denied(method, rule));
    }
    Ok(())
}

/// [`Rule::Decline`]: through the web listener, `TransitionChange` only
/// declines, by a person, at the commit the person saw, naming no
/// workspace, so a page cannot propose, withdraw or supersede for an
/// agent, decline a tree nobody looked at, or record a workspace's session
/// on a person's decline.
pub(crate) fn check_decline(
    web: bool,
    request: &TransitionChangeRequest,
) -> Result<(), super::WebError> {
    let refused = request.to() != LifecycleState::Declined
        || request.expected_commit_id.trim().is_empty()
        || !person(request.actor())
        || !request.workspace.is_empty();
    refuse(
        web,
        refused,
        "/reef.api.v1.ChangeService/TransitionChange",
        Rule::Decline,
    )
}

/// [`Rule::Comment`]: through the web listener, `CreateComment` writes a
/// person's note, instruction or answer. It never asks for help, which is
/// an agent's, never speaks as a workspace's or a client's session, and
/// never confirms an instruction in the same call: the page shows the
/// estimate first.
pub(crate) fn check_comment(
    web: bool,
    request: &CreateCommentRequest,
) -> Result<(), super::WebError> {
    let refused = !person(request.actor())
        || request.kind() == CommentKind::HelpRequest
        || request.confirm
        || !request.workspace.is_empty()
        || !request.client_session.is_empty();
    refuse(
        web,
        refused,
        "/reef.api.v1.CommentService/CreateComment",
        Rule::Comment,
    )
}

/// [`Rule::Confirm`]'s part a request shows: naming no workspace. That the
/// move is a confirm or a cancel needs the instruction's state, so
/// `crate::comments::update` checks it.
pub(crate) fn check_confirm(
    web: bool,
    request: &UpdateInstructionRequest,
) -> Result<(), super::WebError> {
    refuse(
        web,
        !request.workspace.is_empty(),
        "/reef.api.v1.CommentService/UpdateInstruction",
        Rule::Confirm,
    )
}

/// [`Rule::Land`]: through the web listener, `LandChange` lands what a
/// person saw: by a person, at a named commit, naming no workspace.
pub(crate) fn check_land(web: bool, request: &LandChangeRequest) -> Result<(), super::WebError> {
    // A request naming no revision is the handler's to refuse; it still
    // names no workspace, whose refresh the handler would run first.
    let refused = !request.workspace.is_empty()
        || (!request.revision.trim().is_empty()
            && (request.expected_commit_id.trim().is_empty() || !person(request.actor())));
    refuse(
        web,
        refused,
        "/reef.api.v1.LandingService/LandChange",
        Rule::Land,
    )
}

/// The rule of the write at `path`, when the web listener passes it.
pub(crate) fn rule(path: &str) -> Option<Rule> {
    TABLE
        .iter()
        .find(|(method, _)| *method == path)
        .map(|(_, rule)| *rule)
}

#[cfg(test)]
mod tests {
    use reef_api::v1::Access;

    use super::*;

    #[test]
    fn rows_are_writes_and_not_issuance() {
        for (path, _) in TABLE {
            assert_eq!(reef_api::access(path), Some(Access::Write), "{path}");
        }
        assert_eq!(rule("/reef.api.v1.IdentityService/IssueSessionKey"), None);
    }

    #[test]
    fn web_transitions_only_decline_by_a_person_at_a_commit() {
        let decline = TransitionChangeRequest {
            to: LifecycleState::Declined.into(),
            expected_commit_id: "0123abcd".to_owned(),
            ..TransitionChangeRequest::default()
        };
        assert!(check_decline(true, &decline).is_ok());
        for refused in [
            TransitionChangeRequest {
                to: LifecycleState::Withdrawn.into(),
                ..decline.clone()
            },
            TransitionChangeRequest {
                expected_commit_id: " ".to_owned(),
                ..decline.clone()
            },
            TransitionChangeRequest {
                actor: TransitionActor::Agent.into(),
                ..decline.clone()
            },
            TransitionChangeRequest {
                workspace: "6ce393d29a64e06c".to_owned(),
                ..decline.clone()
            },
        ] {
            assert!(check_decline(true, &refused).is_err(), "{refused:?}");
            assert!(
                check_decline(false, &refused).is_ok(),
                "the socket is not the web"
            );
        }
    }

    #[test]
    fn web_landings_are_a_persons_at_a_commit() {
        let land = LandChangeRequest {
            revision: "0123abcd".to_owned(),
            expected_commit_id: "0123abcd".to_owned(),
            ..LandChangeRequest::default()
        };
        assert!(check_land(true, &land).is_ok());
        for refused in [
            LandChangeRequest {
                expected_commit_id: String::new(),
                ..land.clone()
            },
            LandChangeRequest {
                actor: TransitionActor::Agent.into(),
                ..land.clone()
            },
            LandChangeRequest {
                workspace: "6ce393d29a64e06c".to_owned(),
                ..land.clone()
            },
            LandChangeRequest {
                workspace: "6ce393d29a64e06c".to_owned(),
                ..LandChangeRequest::default()
            },
        ] {
            assert!(check_land(true, &refused).is_err(), "{refused:?}");
            assert!(
                check_land(false, &refused).is_ok(),
                "the socket is not the web"
            );
        }
        let confirm = UpdateInstructionRequest {
            workspace: "6ce393d29a64e06c".to_owned(),
            ..UpdateInstructionRequest::default()
        };
        assert!(check_confirm(true, &confirm).is_err());
        assert!(check_confirm(false, &confirm).is_ok());
    }

    #[test]
    fn web_comments_are_a_persons_and_unconfirmed() {
        let note = CreateCommentRequest {
            kind: CommentKind::Note.into(),
            text: "looks right".to_owned(),
            ..CreateCommentRequest::default()
        };
        assert!(check_comment(true, &note).is_ok());
        for refused in [
            CreateCommentRequest {
                actor: TransitionActor::Agent.into(),
                ..note.clone()
            },
            CreateCommentRequest {
                kind: CommentKind::HelpRequest.into(),
                ..note.clone()
            },
            CreateCommentRequest {
                confirm: true,
                ..note.clone()
            },
            CreateCommentRequest {
                workspace: "6ce393d29a64e06c".to_owned(),
                ..note.clone()
            },
            CreateCommentRequest {
                client_session: "s-1".to_owned(),
                ..note.clone()
            },
        ] {
            assert!(check_comment(true, &refused).is_err(), "{refused:?}");
            assert!(
                check_comment(false, &refused).is_ok(),
                "the socket is not the web"
            );
        }
    }
}
