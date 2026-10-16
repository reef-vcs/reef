//! The copy backend: `std::fs::copy` per file with mtimes restored, the
//! baseline and the backend of last resort. R1.07 implements it; until then
//! it is selectable (its probe says available, so `auto` resolves to it) but
//! every operation fails with [`BackendError::Unavailable`].

use super::{
    BackendError, BackendKind, BaseEntry, DiskUsage, SwapOutcome, WorkspacePaths, not_implemented,
};
use crate::backend::Backend;

/// The copy backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyBackend;

impl Backend for CopyBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Copy
    }

    fn materialize(&self, _: &BaseEntry, _: &WorkspacePaths) -> Result<(), BackendError> {
        Err(not_implemented(BackendKind::Copy, "R1.07"))
    }

    fn disk_usage(&self, _: &BaseEntry, _: &WorkspacePaths) -> Result<DiskUsage, BackendError> {
        Err(not_implemented(BackendKind::Copy, "R1.07"))
    }

    fn swap_base(
        &self,
        _: &WorkspacePaths,
        _: &BaseEntry,
        _: &BaseEntry,
    ) -> Result<SwapOutcome, BackendError> {
        Err(not_implemented(BackendKind::Copy, "R1.07"))
    }

    fn tear_down(&self, _: &WorkspacePaths) -> Result<(), BackendError> {
        Err(not_implemented(BackendKind::Copy, "R1.07"))
    }
}
