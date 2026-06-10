//! Implements the synchronous `PackHost` trait against the daemon's services.
//! Runs inside `tokio::task::spawn_blocking`; bridges to async brain ops via a
//! captured runtime Handle. Artifact repo + skills + fs are synchronous.

use std::path::Path;
use std::sync::Arc;

use nevoflux_pack::error::{PackError, PackResult};
use nevoflux_pack::host::{
    ArtifactSpec, ImportOutcome, PackHost, PackProgress as EnginePackProgress, PackUnlock,
    ReloadReport,
};
use nevoflux_pack::paths::ResolvedPaths;
use nevoflux_pack::receipt::{FileReceipt, Receipt};
use nevoflux_skills::SkillRegistry;
use nevoflux_storage::Database;
use tokio::sync::RwLock;

/// Daemon-side implementation of [`PackHost`].
///
/// The trait is synchronous but brain ops are async; this impl runs inside
/// `tokio::task::spawn_blocking` and bridges to async via the captured
/// runtime [`handle`](Self::handle). Fields are `pub(crate)` so `pack::rpc`
/// (and the integration test, via the public re-export) can construct it.
pub struct PackHostImpl {
    pub(crate) paths: ResolvedPaths,
    pub(crate) db: Arc<Database>,
    pub(crate) skills: Arc<RwLock<SkillRegistry>>,
    pub(crate) brain: Option<Arc<dyn nevoflux_brain::BrainEngine>>,
    pub(crate) bus: Option<Arc<crate::event_bus::EventBus>>,
    pub(crate) handle: tokio::runtime::Handle,
    pub(crate) op_id: String,
}

impl PackHostImpl {
    fn io_err(ctx: &str, e: impl std::fmt::Display) -> PackError {
        PackError::Host(format!("{ctx}: {e}"))
    }
}

impl PackHost for PackHostImpl {
    fn resolved_paths(&self) -> &ResolvedPaths {
        &self.paths
    }

    fn place_file(&self, dest: &Path, bytes: &[u8]) -> PackResult<FileReceipt> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Self::io_err("create_dir_all", e))?;
        }
        std::fs::write(dest, bytes).map_err(|e| Self::io_err("write", e))?;
        Ok(FileReceipt {
            path: dest.to_path_buf(),
            sha256: Receipt::sha256_hex(bytes),
        })
    }

    fn remove_file(&self, path: &Path) -> PackResult<()> {
        if path.exists() {
            std::fs::remove_file(path).map_err(|e| Self::io_err("remove_file", e))?;
        }
        // Prune now-empty parent dirs, but never climb above the config dir.
        let stop = self.paths.config_dir.as_path();
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if dir == stop || !dir.starts_with(stop) {
                break;
            }
            if std::fs::read_dir(dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(dir);
                cur = dir.parent();
            } else {
                break;
            }
        }
        Ok(())
    }

    fn read_receipt(&self, pack: &str) -> PackResult<Option<Receipt>> {
        let p = self.paths.receipt_path(pack);
        match std::fs::read_to_string(&p) {
            Ok(s) => {
                let r = serde_json::from_str(&s).map_err(|e| Self::io_err("parse receipt", e))?;
                Ok(Some(r))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Self::io_err("read receipt", e)),
        }
    }

    fn write_receipt(&self, pack: &str, receipt: &Receipt) -> PackResult<()> {
        let p = self.paths.receipt_path(pack);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Self::io_err("mkdir receipt", e))?;
        }
        let s =
            serde_json::to_string_pretty(receipt).map_err(|e| Self::io_err("ser receipt", e))?;
        std::fs::write(&p, s).map_err(|e| Self::io_err("write receipt", e))
    }

    fn delete_receipt(&self, pack: &str) -> PackResult<()> {
        let p = self.paths.receipt_path(pack);
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| Self::io_err("rm receipt", e))?;
        }
        Ok(())
    }

    // --- filled in C2 (artifacts), C3 (brain), C4 (skills/progress) ---
    fn page_exists(&self, _slug: &str) -> PackResult<bool> {
        unimplemented!("C3")
    }
    fn put_page(&self, _slug: &str, _body: &str) -> PackResult<()> {
        unimplemented!("C3")
    }
    fn delete_page(&self, _slug: &str) -> PackResult<()> {
        unimplemented!("C3")
    }
    fn import_source(&self, _n: &str, _b: &[u8], _u: &PackUnlock) -> PackResult<ImportOutcome> {
        Err(PackError::Host(
            "knowledge import is deferred until gbrain source-mapping lands (M5)".into(),
        ))
    }
    fn remove_source(&self, _n: &str) -> PackResult<()> {
        Err(PackError::Host(
            "knowledge source removal deferred (M5)".into(),
        ))
    }
    fn upsert_artifact(&self, _spec: &ArtifactSpec) -> PackResult<()> {
        unimplemented!("C2")
    }
    fn remove_artifact(&self, _id: &str) -> PackResult<()> {
        unimplemented!("C2")
    }
    fn reload_skills(&self) -> PackResult<ReloadReport> {
        unimplemented!("C4")
    }
    fn report(&self, _p: EnginePackProgress) { /* filled in C4 */
    }
}
