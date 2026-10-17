//! The copy backend: `std::fs::copy` per file with modes and mtimes
//! restored, the baseline and the backend of last resort (R1.07). It works
//! on every filesystem and costs time and disk in proportion to the tree.
//!
//! `std::fs::copy` may itself clone (`fclonefileat` on APFS,
//! `copy_file_range` on btrfs and XFS); a free clone is fine for the
//! fallback. The clone backends (R1.08, R1.10) do not use it, because they
//! must fail loudly when cloning is impossible.

use std::io;
use std::path::Path;

use super::fs_util::{
    FileCopier, allocated_bytes, changed_files_vs_base, copy_tree, remove_tree_in_background,
};
use super::{BackendError, BackendKind, BaseEntry, DiskUsage, SwapOutcome, WorkspacePaths};
use crate::backend::Backend;

/// Full-copy materialization. Works everywhere; costs O(tree) time and disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyBackend;

impl FileCopier for CopyBackend {
    fn copy_file(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::copy(from, to).map(drop)
    }
}

impl Backend for CopyBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Copy
    }

    /// Copies the base's tree into `ws.root`. A failure part way leaves a
    /// partial tree, which the daemon tears down (R1.05).
    fn materialize(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<(), BackendError> {
        if std::fs::symlink_metadata(&ws.root).is_ok() {
            return Err(BackendError::RootExists(ws.root.clone()));
        }
        let counts = copy_tree(&base.path, &ws.root, self)?;
        tracing::debug!(
            path = %ws.root.display(),
            file_count = counts.files,
            symlink_count = counts.symlinks,
            "workspace copied"
        );
        Ok(())
    }

    /// The whole tree's allocated bytes, since a copy shares nothing with
    /// its base (unless `std::fs::copy` cloned, which this does not see),
    /// and the files that differ from the base.
    fn disk_usage(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<DiskUsage, BackendError> {
        let changed = changed_files_vs_base(base, ws)?;
        Ok(DiskUsage {
            allocated_bytes: allocated_bytes(&ws.root)?,
            files_changed: changed.files_changed,
        })
    }

    /// Nothing to do on disk: the daemon applies the tree diff with a jj
    /// check-out.
    fn swap_base(
        &self,
        _: &WorkspacePaths,
        _: &BaseEntry,
        _: &BaseEntry,
    ) -> Result<SwapOutcome, BackendError> {
        Ok(SwapOutcome::NeedsCheckout)
    }

    /// Renames the root to a `.trash-*` sibling and deletes it in the
    /// background (inline without a tokio runtime). Returns once the root
    /// is gone.
    fn tear_down(&self, ws: &WorkspacePaths) -> Result<(), BackendError> {
        match remove_tree_in_background(&ws.root) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(BackendError::RootMissing(ws.root.clone()))
            }
            Err(source) => Err(BackendError::Io {
                op: "move to trash",
                path: ws.root.clone(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use reef_lib::error::{Classify as _, ErrorKind};

    use super::*;
    use crate::backend::fs_util::free_bytes;
    use crate::model::RepoId;

    /// A base entry over `tree`, with an empty `tree_state` beside it (an
    /// empty protobuf is a state with no files).
    fn base_over(tree: &Path) -> BaseEntry {
        let state_dir = tree.with_extension("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("tree_state"), b"").unwrap();
        BaseEntry {
            repo_id: RepoId::generate(),
            tree_id: "00".into(),
            path: tree.to_owned(),
            state_dir,
            bytes: 0,
            files: 0,
        }
    }

    #[test]
    fn copy_backend_materializes_small_tree() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("bases/tree");
        let mut written = 0;
        for index in 0..100 {
            let path = tree.join(format!("d{}/f{index}.txt", index % 7));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let contents = vec![b"abcdefghijklmnopqrstuvwxyz"[index % 26]; 4096 + index];
            written += contents.len() as u64;
            std::fs::write(path, contents).unwrap();
        }
        let base = base_over(&tree);
        let ws = WorkspacePaths::under(
            &dir.path().join("repo"),
            "00000000000000aa".parse().unwrap(),
        );

        CopyBackend.materialize(&base, &ws).unwrap();
        assert_eq!(
            std::fs::read(ws.root.join("d3/f10.txt")).unwrap(),
            std::fs::read(tree.join("d3/f10.txt")).unwrap()
        );
        let usage = CopyBackend.disk_usage(&base, &ws).unwrap();
        assert!(usage.allocated_bytes >= written, "{usage:?} vs {written}");
        assert_eq!(
            usage.files_changed, 100,
            "the empty base records none of them"
        );

        let error = CopyBackend.materialize(&base, &ws).unwrap_err();
        assert!(matches!(error, BackendError::RootExists(_)), "{error:?}");
        assert_eq!(
            CopyBackend.swap_base(&ws, &base, &base).unwrap(),
            SwapOutcome::NeedsCheckout
        );

        CopyBackend.tear_down(&ws).unwrap();
        assert!(!ws.root.exists());
        let leftovers: Vec<_> = std::fs::read_dir(ws.root.parent().unwrap())
            .unwrap()
            .collect();
        assert!(leftovers.is_empty(), "deleted inline without a runtime");
        let error = CopyBackend.tear_down(&ws).unwrap_err();
        assert!(matches!(error, BackendError::RootMissing(_)), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(tree.join("d3/f10.txt").is_file(), "the base is untouched");
    }

    /// The baseline R1.12 compares every backend with: the copy walk over
    /// the `large` synthetic preset (`REEF_TEST_PRESET` picks another).
    /// Prints wall time and the free-space delta. Run by hand:
    /// `cargo test -p reef-daemon --release copy_backend_preset_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "generates the large preset; run by hand to record the baseline"]
    fn copy_backend_preset_timing() {
        use reef_testing::synth::{self, Preset, Spec};

        let preset = match std::env::var("REEF_TEST_PRESET").as_deref() {
            Ok("small") => Preset::Small,
            Ok("medium") => Preset::Medium,
            _ => Preset::Large,
        };
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("bases/tree");
        let manifest = synth::generate(&tree, &Spec::preset(preset), 42, false).unwrap();
        let base = base_over(&tree);
        let ws = WorkspacePaths::under(
            &dir.path().join("repo"),
            "00000000000000aa".parse().unwrap(),
        );
        std::fs::create_dir_all(ws.root.parent().unwrap()).unwrap();

        let free_before = free_bytes(dir.path()).unwrap();
        let begun = Instant::now();
        CopyBackend.materialize(&base, &ws).unwrap();
        let elapsed = begun.elapsed();
        let free_after = free_bytes(dir.path()).unwrap();
        eprintln!(
            "copy backend, {preset:?} preset ({} files): materialize {elapsed:.2?}, free space -{} MiB",
            manifest.files,
            free_before.saturating_sub(free_after) >> 20
        );
    }
}
