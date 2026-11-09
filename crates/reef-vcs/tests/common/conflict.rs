//! Conflicts made outside the daemon, as a user's `jj rebase` would (R3.25).

use std::path::Path;
use std::sync::Arc;

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _};

/// The repository at `root`, loaded at its head.
fn load(root: &Path) -> Arc<ReadonlyRepo> {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    let settings = reef_daemon::testing::test_settings();
    let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
        .create(root)
        .unwrap()
        .load(
            &settings,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .unwrap();
    pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap()
}

/// `commit` rebased onto `onto`, recorded as a rewrite; the new commit's
/// id.
pub(crate) fn rebase(root: &Path, commit: &str, onto: &str) -> String {
    let repo = load(root);
    let mut tx = repo.start_transaction();
    let store = tx.repo().store().clone();
    let old = pollster::block_on(store.get_commit_async(&CommitId::try_from_hex(commit).unwrap()))
        .unwrap();
    let new = pollster::block_on(jj_lib::rewrite::rebase_commit(
        tx.repo_mut(),
        old.clone(),
        vec![CommitId::try_from_hex(onto).unwrap()],
    ))
    .unwrap();
    tx.repo_mut()
        .set_rewritten_commit(old.id().clone(), new.id().clone());
    pollster::block_on(tx.repo_mut().rebase_descendants()).unwrap();
    pollster::block_on(tx.commit("rebase")).unwrap();
    new.id().hex()
}
