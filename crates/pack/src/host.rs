//! The single seam between the pure engine and the platform. The daemon
//! implements this (Plan 02); tests use MockPackHost.

use std::path::Path;

use crate::error::PackResult;
use crate::paths::ResolvedPaths;
use crate::receipt::{FileReceipt, Receipt};

/// Material to unlock a shipped `.nbrain` bundle (decoded by the host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackUnlock {
    /// 64-char lowercase hex of a 32-byte key.
    KeyHex(String),
    Password(String),
}

/// A prebuilt persistent project artifact (the dashboard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSpec {
    pub artifact_id: String,
    pub content_type: String,
    pub entry: String,
    /// filename -> file contents (the canvas-app bundle).
    pub files: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    pub loaded: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    pub source_name: String,
    pub pages_imported: u64,
}

/// Lifecycle phase, for progress reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackPhase {
    Resolve,
    Compat,
    Capability,
    Idempotency,
    Place,
    Seed,
    Knowledge,
    Artifact,
    Activate,
    Commit,
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseStatus {
    Running,
    Ok,
    Failed,
    RolledBack,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackProgress {
    pub phase: PackPhase,
    pub status: PhaseStatus,
    pub progress_pct: u8,
    pub log: String,
}

/// Every method is individually reversible; the lifecycle records each result
/// in the receipt so a mid-install failure rolls back. Synchronous: the daemon
/// impl bridges to async internally (Plan 02).
pub trait PackHost {
    fn resolved_paths(&self) -> &ResolvedPaths;

    // Files
    fn place_file(&self, dest: &Path, bytes: &[u8]) -> PackResult<FileReceipt>;
    fn remove_file(&self, path: &Path) -> PackResult<()>;

    // Seed (GBrain), idempotent
    fn page_exists(&self, slug: &str) -> PackResult<bool>;
    fn put_page(&self, slug: &str, body: &str) -> PackResult<()>;
    fn delete_page(&self, slug: &str) -> PackResult<()>;

    // Knowledge (GBrain ReadOnly source)
    fn import_source(
        &self,
        source_name: &str,
        bundle_bytes: &[u8],
        unlock: &PackUnlock,
    ) -> PackResult<ImportOutcome>;
    fn remove_source(&self, source_name: &str) -> PackResult<()>;

    // Dashboard artifact
    fn upsert_artifact(&self, spec: &ArtifactSpec) -> PackResult<()>;
    fn remove_artifact(&self, id: &str) -> PackResult<()>;

    // Activation
    fn reload_skills(&self) -> PackResult<ReloadReport>;

    // Receipt persistence
    fn read_receipt(&self, pack: &str) -> PackResult<Option<Receipt>>;
    fn write_receipt(&self, pack: &str, receipt: &Receipt) -> PackResult<()>;
    fn delete_receipt(&self, pack: &str) -> PackResult<()>;

    // Progress callback (daemon forwards to EventBus).
    fn report(&self, progress: PackProgress);
}
