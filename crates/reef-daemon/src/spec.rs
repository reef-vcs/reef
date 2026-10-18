//! What the daemon takes from a repository's `reef.toml` (R1.19, ADR 0021).
//!
//! [`Manager::create`](crate::workspace::Manager::create) reads the spec
//! from the target commit's tree with `Repo::spec_at` on the repository
//! thread and keeps the `ResolvedSpec`'s snapshot spec on the workspace's
//! entry; daemon start reads it again from the workspace's base tree. A
//! `reef.toml` edited inside a workspace takes effect for workspaces
//! created later, never for the running one.

use std::time::Duration;

use reef_lib::spec::Spec;

use crate::workspace::SnapshotSpec;

/// What the daemon derives from a spec for one commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResolvedSpec {
    /// The spec; [`Spec::default`] when the tree has no `reef.toml`. R1.20
    /// reads its stores.
    pub spec: Spec,
    /// `[workspace] lease`, for creates that do not ask for a length. The
    /// caller caps it at `lease.max`.
    pub lease: Option<Duration>,
    /// What the workspace's snapshots and watcher exclude: the spec's
    /// `[snapshot] exclude` entries and every store mount, which the ignore
    /// chain anchors at the root the way [`Spec::effective_excludes`] does.
    pub snapshot: SnapshotSpec,
}

/// The [`ResolvedSpec`] of `spec`, or of the defaults when the tree has no
/// `reef.toml`.
pub(crate) fn resolve(spec: Option<Spec>) -> ResolvedSpec {
    let spec = spec.unwrap_or_default();
    ResolvedSpec {
        lease: spec.workspace.lease,
        snapshot: SnapshotSpec {
            excludes: spec.snapshot.exclude.clone(),
            mounts: spec
                .stores
                .iter()
                .map(|store| store.mount.clone())
                .collect(),
        },
        spec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_spec_is_the_defaults() {
        assert_eq!(resolve(None), ResolvedSpec::default());
    }

    #[test]
    fn mounts_are_excluded() {
        let spec = Spec::parse(
            "[workspace]\nlease = \"2h\"\n[snapshot]\nexclude = [\"target/\"]\n\
             [[store]]\nname = \"deps\"\nlockfile = \"deps.lock\"\nmount = \"vendor/deps\"\nbuild = [\"true\"]\n",
        )
        .unwrap();
        let resolved = resolve(Some(spec));
        assert_eq!(resolved.lease, Some(Duration::from_hours(2)));
        assert_eq!(resolved.snapshot.excludes, ["target/"]);
        assert_eq!(resolved.snapshot.mounts, ["vendor/deps"]);
        let lines = reef_lib::jj::ignore::literal_prefixes(
            resolved
                .snapshot
                .excludes
                .iter()
                .chain(&resolved.snapshot.mounts)
                .map(String::as_str),
        );
        assert_eq!(lines, ["target", "vendor/deps"]);
    }
}
