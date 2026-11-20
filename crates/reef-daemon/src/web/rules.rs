//! The web listener's allowlist of writes (R3.43, ADR 0085): beside every
//! `ACCESS_READ` method, step 8 of the gate passes the methods of
//! [`TABLE`], and refuses every other WRITE and every ADMIN method with
//! `WEB_ACCESS_DENIED`. The gate sees only encoded bytes, so each row's
//! rule is the handler's to check, with the request's [`super::Caller`];
//! the rows are what a browser holding a session may change.
//!
//! `IssueSessionKey` is not here: a browser gets a key only through an
//! approval, bound to one sign-off. Publishing, fetching, pushing and
//! creating workspaces, which run the user's git credentials or tools,
//! never are.

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
];

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
}
