//! The v1 RPCs whose ticket has not landed (R2.01). Each answers
//! `UNIMPLEMENTED` with kind `Internal` and the hint `not implemented until
//! <ticket>`, so the schema can be reviewed, and clients written, before the
//! server code exists. A ticket that implements an RPC removes its row.
//!
//! Every handler of a stubbed RPC, in its service's module, is
//! `Err(stub("<method>"))`.

use std::pin::Pin;

use futures::Stream;
use reef_api::advice::Advice;
use reef_api::error::{ErrorKind, kind_name};
use reef_api::proto::v1::ErrorDetail;
use tonic::{Code, Status};

use crate::error::status_with_detail;

/// Every v1 RPC whose ticket has not landed, by full method name, and the
/// ticket. Removed row by row as they land.
pub const STUBS: &[(&str, &str)] = &[
    ("reef.daemon.v1.Workspaces/Status", "R2.02"),
    ("reef.daemon.v1.Changes/Log", "R2.02"),
    ("reef.daemon.v1.Changes/Get", "R2.02"),
    ("reef.daemon.v1.Changes/Diff", "R2.02"),
    ("reef.daemon.v1.Changes/Describe", "R2.03"),
    ("reef.daemon.v1.Changes/Abandon", "R2.03"),
    ("reef.daemon.v1.Changes/Squash", "R2.03"),
    ("reef.daemon.v1.Operations/Log", "R2.02"),
    ("reef.daemon.v1.Operations/Get", "R2.02"),
    ("reef.daemon.v1.Operations/Undo", "R2.03"),
    ("reef.daemon.v1.Operations/Subscribe", "R2.09"),
    ("reef.daemon.v1.Operations/Replay", "R2.12"),
    ("reef.daemon.v1.Trees/List", "R2.04"),
    ("reef.daemon.v1.Trees/Read", "R2.04"),
    ("reef.daemon.v1.Trees/Apply", "R2.04"),
    ("reef.daemon.v1.Presence/Subscribe", "R2.10"),
    ("reef.daemon.v1.Presence/Report", "R2.10"),
    ("reef.daemon.v1.Symbols/Index", "R2.14"),
    ("reef.daemon.v1.Symbols/Ingest", "R2.14"),
    ("reef.daemon.v1.Symbols/Lookup", "R2.14"),
    ("reef.daemon.v1.Queries/List", "R2.16"),
    ("reef.daemon.v1.Queries/Run", "R2.16"),
    ("reef.daemon.v1.Remote/IssueToken", "R2.08"),
    ("reef.daemon.v1.Remote/ListTokens", "R2.08"),
    ("reef.daemon.v1.Remote/RevokeToken", "R2.08"),
    ("reef.daemon.v1.Stores/Prepare", "R2.27"),
    ("reef.daemon.v1.Stores/Seal", "R2.27"),
    ("reef.daemon.v1.Stores/Abort", "R2.27"),
];

/// The ticket that implements `method` (`reef.daemon.v1.<Service>/<Rpc>`),
/// if it is a stub.
pub fn ticket_of(method: &str) -> Option<&'static str> {
    STUBS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, ticket)| *ticket)
}

/// The status a stub answers: `UNIMPLEMENTED`, whose detail has kind
/// `Internal` (so a client maps it to exit 1, as R1.22's CLI stubs did) and
/// the hint `not implemented until <ticket>`.
pub fn stub(method: &str) -> Status {
    let ticket = ticket_of(method).unwrap_or("a later ticket");
    let message = format!("{method} is not implemented until {ticket}");
    let advice = Advice::hint(format!("not implemented until {ticket}"));
    let detail = ErrorDetail {
        kind: kind_name(ErrorKind::Internal).to_owned(),
        chain: vec![message.clone()],
        hint: advice.hint,
        next: advice.next,
    };
    status_with_detail(Code::Unimplemented, message, &detail)
}

/// A server stream no stub ever yields from.
pub(super) type StubStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stub_names_its_ticket() {
        let status = stub("reef.daemon.v1.Trees/Apply");
        assert_eq!(status.code(), Code::Unimplemented);
        assert_eq!(
            status.message(),
            "reef.daemon.v1.Trees/Apply is not implemented until R2.04"
        );
        let detail = reef_api::error::detail_of(&status).unwrap();
        assert_eq!(detail.kind, "Internal");
        assert_eq!(detail.hint, "not implemented until R2.04");
        assert_eq!(
            reef_api::error::kind_from_status(&status),
            ErrorKind::Internal
        );
    }

    #[test]
    fn every_stub_is_one_row() {
        let mut names: Vec<&str> = STUBS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), STUBS.len());
    }
}
