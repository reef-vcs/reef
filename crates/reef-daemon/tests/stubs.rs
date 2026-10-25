//! The RPCs whose ticket has not landed (R2.01): each answers
//! `UNIMPLEMENTED` naming its ticket, and every v1 method is either one of
//! them or implemented.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]
#![allow(
    clippy::default_trait_access,
    reason = "every stub is called with an empty request, whatever its type"
)]

use std::collections::BTreeSet;

use prost::Message as _;
use reef_api::client::{Client, ClientError};
use reef_api::error::{Classify, ErrorKind};
use reef_daemon::service::stubs::STUBS;
use reef_daemon::testing::TestDaemon;

/// The v1 methods with server code, each exercised by the integration
/// tests of its ticket.
const IMPLEMENTED: &[&str] = &[
    "reef.daemon.v1.Daemon/Status",
    "reef.daemon.v1.Repositories/Register",
    "reef.daemon.v1.Repositories/List",
    "reef.daemon.v1.Repositories/Forget",
    "reef.daemon.v1.Workspaces/Create",
    "reef.daemon.v1.Workspaces/List",
    "reef.daemon.v1.Workspaces/Inspect",
    "reef.daemon.v1.Workspaces/Release",
    "reef.daemon.v1.Workspaces/Snapshot",
    "reef.daemon.v1.Workspaces/Heartbeat",
    "reef.daemon.v1.Workspaces/Move",
    "reef.daemon.v1.Maintenance/Gc",
    "reef.daemon.v1.Maintenance/Backup",
    "reef.daemon.v1.Maintenance/Quiesce",
    "reef.daemon.v1.Maintenance/Resume",
    "reef.daemon.v1.Stores/Build",
    "reef.daemon.v1.Stores/List",
];

/// Calls `method` through the client with an empty request.
async fn call(client: &Client, method: &str) -> Result<(), ClientError> {
    match method {
        "reef.daemon.v1.Workspaces/Status" => {
            client.workspace_status(Default::default()).await.map(drop)
        }
        "reef.daemon.v1.Changes/Log" => client.log_changes(Default::default()).await.map(drop),
        "reef.daemon.v1.Changes/Get" => client.get_change(Default::default()).await.map(drop),
        "reef.daemon.v1.Changes/Diff" => client.diff(Default::default()).await.map(drop),
        "reef.daemon.v1.Changes/Describe" => {
            client.describe_change(Default::default()).await.map(drop)
        }
        "reef.daemon.v1.Changes/Abandon" => {
            client.abandon_change(Default::default()).await.map(drop)
        }
        "reef.daemon.v1.Changes/Squash" => client.squash_change(Default::default()).await.map(drop),
        "reef.daemon.v1.Operations/Log" => {
            client.log_operations(Default::default()).await.map(drop)
        }
        "reef.daemon.v1.Operations/Get" => client.get_operation(Default::default()).await.map(drop),
        "reef.daemon.v1.Operations/Undo" => client.undo(Default::default()).await.map(drop),
        "reef.daemon.v1.Operations/Subscribe" => {
            client.subscribe(Default::default()).await.map(drop)
        }
        "reef.daemon.v1.Operations/Replay" => client.replay(Default::default()).await.map(drop),
        "reef.daemon.v1.Trees/List" => client.list_tree(Default::default()).await.map(drop),
        "reef.daemon.v1.Trees/Read" => client.read_file(Default::default()).await.map(drop),
        "reef.daemon.v1.Trees/Apply" => client.apply_edits(tokio_stream::empty()).await.map(drop),
        "reef.daemon.v1.Presence/Subscribe" => client
            .subscribe_presence(Default::default())
            .await
            .map(drop),
        "reef.daemon.v1.Presence/Report" => client
            .report_presence(tokio_stream::empty())
            .await
            .map(drop),
        "reef.daemon.v1.Symbols/Index" => client.index_symbols(Default::default()).await.map(drop),
        "reef.daemon.v1.Symbols/Ingest" => {
            client.ingest_symbols(tokio_stream::empty()).await.map(drop)
        }
        "reef.daemon.v1.Symbols/Lookup" => {
            client.lookup_symbols(Default::default()).await.map(drop)
        }
        "reef.daemon.v1.Queries/List" => client.list_queries().await.map(drop),
        "reef.daemon.v1.Queries/Run" => client.run_query(Default::default()).await.map(drop),
        "reef.daemon.v1.Remote/IssueToken" => {
            client.issue_token(Default::default()).await.map(drop)
        }
        "reef.daemon.v1.Remote/ListTokens" => client.list_tokens().await.map(drop),
        "reef.daemon.v1.Remote/RevokeToken" => {
            client.revoke_token(Default::default()).await.map(drop)
        }
        "reef.daemon.v1.Stores/Prepare" => client
            .prepare_store_build(Default::default())
            .await
            .map(drop),
        "reef.daemon.v1.Stores/Seal" => client.seal_store_build(Default::default()).await.map(drop),
        "reef.daemon.v1.Stores/Abort" => {
            client.abort_store_build(Default::default()).await.map(drop)
        }
        other => panic!("no client call for {other}; add one here"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_stub_answers_unimplemented_with_its_ticket() {
    let daemon = TestDaemon::start().await;
    for (method, ticket) in STUBS {
        let error = call(&daemon.client, method).await.expect_err(method);
        let ClientError::Rpc { status, .. } = &error else {
            panic!("{method}: {error:?}");
        };
        assert_eq!(status.code(), tonic::Code::Unimplemented, "{method}");
        assert_eq!(error.kind(), ErrorKind::Internal, "{method}");
        assert_eq!(
            error.advice().map(|advice| advice.hint),
            Some(format!("not implemented until {ticket}")),
            "{method}"
        );
    }
}

#[test]
fn every_v1_method_is_a_stub_or_implemented() {
    let set = prost_types::FileDescriptorSet::decode(reef_api::DESCRIPTOR_SET).unwrap();
    let methods: BTreeSet<String> = set
        .file
        .iter()
        .filter(|file| file.package() == "reef.daemon.v1")
        .flat_map(|file| &file.service)
        .flat_map(|service| {
            service
                .method
                .iter()
                .map(move |m| format!("reef.daemon.v1.{}/{}", service.name(), m.name()))
        })
        .collect();
    let stubs: BTreeSet<String> = STUBS.iter().map(|(m, _)| (*m).to_owned()).collect();
    let implemented: BTreeSet<String> = IMPLEMENTED.iter().map(|m| (*m).to_owned()).collect();
    assert!(
        stubs.is_disjoint(&implemented),
        "{:?}",
        stubs.intersection(&implemented)
    );
    let known: BTreeSet<String> = stubs.union(&implemented).cloned().collect();
    assert_eq!(methods, known);
}
