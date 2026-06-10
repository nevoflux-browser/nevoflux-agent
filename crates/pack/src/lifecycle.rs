//! Transactional install / uninstall / update over a PackHost.

use std::path::{Path, PathBuf};

use crate::capability;
use crate::error::{PackError, PackResult};
use crate::host::{
    ArtifactSpec, PackHost, PackPhase, PackProgress, PackUnlock, PhaseStatus,
};
use crate::manifest::{Manifest, UnlockSpec};
use crate::receipt::{PathsSource, Receipt, RECEIPT_VERSION};

#[derive(Debug, Clone, Default)]
pub struct InstallOpts {
    pub force: bool,
    /// Caller-supplied UTC timestamp (engine is time-free for determinism).
    pub now_utc: String,
    /// Provenance string recorded into the receipt (daemon supplies it for
    /// remote installs; None for local).
    pub source: Option<String>,
    /// sha256 of the source tarball, recorded into the receipt for remote installs.
    pub tarball_sha256: Option<String>,
}

/// Records what was applied so we can roll back in reverse.
#[derive(Default)]
struct Applied {
    files: Vec<PathBuf>,
    pages: Vec<String>,
    sources: Vec<String>,
    artifacts: Vec<String>,
}

fn emit(host: &dyn PackHost, phase: PackPhase, status: PhaseStatus, pct: u8, log: &str) {
    host.report(PackProgress { phase, status, progress_pct: pct, log: log.to_string() });
}

/// Install a pack whose source lives at `pack_dir` (the directory containing
/// `pack.toml`). `raw_toml` is the manifest source (for the config-table scan).
pub fn install(
    host: &dyn PackHost,
    manifest: &Manifest,
    raw_toml: &str,
    pack_dir: &Path,
    opts: &InstallOpts,
) -> PackResult<Receipt> {
    let paths = host.resolved_paths().clone();

    // Phase 1: compat
    emit(host, PackPhase::Compat, PhaseStatus::Running, 5, "checking compatibility");
    if manifest.pack.min_nevoflux > paths.version {
        return Err(PackError::Compat(format!(
            "pack needs nevoflux >= {}, daemon is {}",
            manifest.pack.min_nevoflux, paths.version
        )));
    }

    // Phase 2: capability
    emit(host, PackPhase::Capability, PhaseStatus::Running, 10, "capability check");
    capability::validate(manifest, &paths, raw_toml).map_err(PackError::Capability)?;

    // Phase 3: idempotency
    emit(host, PackPhase::Idempotency, PhaseStatus::Running, 15, "checking existing install");
    if let Some(existing) = host.read_receipt(&manifest.pack.name)? {
        if existing.version == manifest.pack.version && !opts.force {
            return Err(PackError::AlreadyInstalled {
                name: manifest.pack.name.clone(),
                version: existing.version,
            });
        }
    }

    let mut applied = Applied::default();
    let mut receipt = Receipt {
        receipt_version: RECEIPT_VERSION.into(),
        protocol: manifest.pack.protocol.clone(),
        pack: manifest.pack.name.clone(),
        namespace: manifest.namespace().to_string(),
        version: manifest.pack.version.clone(),
        installed_at: opts.now_utc.clone(),
        nevoflux_version: paths.version.clone(),
        paths_source: PathsSource::Daemon,
        files: Vec::new(),
        artifacts: Vec::new(),
        seeded_pages: Vec::new(),
        imported_sources: Vec::new(),
        source: opts.source.clone(),
        tarball_sha256: opts.tarball_sha256.clone(),
    };

    // Run the mutating phases; on any error, roll back and abort.
    let result = (|| -> PackResult<()> {
        place_phase(host, manifest, pack_dir, &paths, &mut receipt, &mut applied)?;
        seed_phase(host, manifest, pack_dir, &mut receipt, &mut applied)?;
        knowledge_phase(host, manifest, pack_dir, &mut receipt, &mut applied)?;
        artifact_phase(host, manifest, pack_dir, &mut receipt, &mut applied)?;
        Ok(())
    })();

    if let Err(e) = result {
        rollback(host, &applied);
        emit(host, PackPhase::Commit, PhaseStatus::RolledBack, 100, &format!("rolled back: {e}"));
        return Err(PackError::RolledBack { reason: e.to_string() });
    }

    // Phase 8: activate (non-fatal)
    emit(host, PackPhase::Activate, PhaseStatus::Running, 90, "reloading skills");
    if let Err(e) = host.reload_skills() {
        emit(host, PackPhase::Activate, PhaseStatus::Failed, 90, &format!("reload warning: {e}"));
    }

    // Phase 9: commit
    host.write_receipt(&manifest.pack.name, &receipt)?;
    emit(host, PackPhase::Commit, PhaseStatus::Ok, 100, "installed");
    Ok(receipt)
}

fn place_phase(
    host: &dyn PackHost,
    manifest: &Manifest,
    pack_dir: &Path,
    paths: &crate::paths::ResolvedPaths,
    receipt: &mut Receipt,
    applied: &mut Applied,
) -> PackResult<()> {
    emit(host, PackPhase::Place, PhaseStatus::Running, 30, "placing files");
    // Skills: flatten one level from pack_dir/<skills.dir> into skills_dir.
    if let Some(s) = &manifest.components.skills {
        let src_root = pack_dir.join(&s.dir);
        for (rel, bytes) in read_dir_flat(&src_root)? {
            let dest = paths.skills_dir.join(&rel);
            // Defense-in-depth: the destination must stay under the whitelisted
            // skills root even if `read_dir_flat` (or a future change) yields an
            // escaping relative name. Capability validation already guards the
            // source path; this guarantees writes can never escape the sandbox.
            assert_under(&dest, &paths.skills_dir)?;
            let fr = host.place_file(&dest, &bytes)?;
            applied.files.push(fr.path.clone());
            receipt.files.push(fr);
        }
    }
    // Canvas-tools: each file flattened to canvas_tools_dir/<basename>.
    if let Some(ct) = &manifest.components.canvas_tools {
        for f in &ct.files {
            let src = pack_dir.join(f);
            let bytes = std::fs::read(&src).map_err(|e| PackError::Host(format!("{}: {e}", src.display())))?;
            let base = Path::new(f).file_name().ok_or_else(|| PackError::Manifest(format!("bad canvas-tool path {f}")))?;
            let dest = paths.canvas_tools_dir.join(base);
            // Defense-in-depth: writes must stay under the canvas-tools root.
            assert_under(&dest, &paths.canvas_tools_dir)?;
            let fr = host.place_file(&dest, &bytes)?;
            applied.files.push(fr.path.clone());
            receipt.files.push(fr);
        }
    }
    Ok(())
}

/// Lexically assert that `dest` is contained within `root` (no FS access). The
/// caller built `dest` by joining a relative name onto `root`, so a normalized
/// `dest` that still starts with `root` cannot have escaped via `..`.
fn assert_under(dest: &Path, root: &Path) -> PackResult<()> {
    let norm = lexical_normalize(dest);
    let root_norm = lexical_normalize(root);
    if norm.starts_with(&root_norm) {
        Ok(())
    } else {
        Err(PackError::Capability(vec![
            crate::capability::Violation::OutsideWhitelistDir {
                component: "place".into(),
                dest: dest.to_path_buf(),
            },
        ]))
    }
}

/// Resolve `.`/`..` components lexically (no FS access, no symlink resolution).
fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn seed_phase(
    host: &dyn PackHost,
    manifest: &Manifest,
    pack_dir: &Path,
    receipt: &mut Receipt,
    applied: &mut Applied,
) -> PackResult<()> {
    emit(host, PackPhase::Seed, PhaseStatus::Running, 50, "seeding pages");
    for s in &manifest.components.seed {
        if host.page_exists(&s.slug)? {
            continue; // idempotent: only-if-absent
        }
        let body = std::fs::read_to_string(pack_dir.join(&s.from))
            .map_err(|e| PackError::Host(format!("seed {}: {e}", s.from)))?;
        host.put_page(&s.slug, &body)?;
        applied.pages.push(s.slug.clone());
        receipt.seeded_pages.push(s.slug.clone());
    }
    Ok(())
}

fn knowledge_phase(
    host: &dyn PackHost,
    manifest: &Manifest,
    pack_dir: &Path,
    receipt: &mut Receipt,
    applied: &mut Applied,
) -> PackResult<()> {
    if let Some(k) = &manifest.components.knowledge {
        emit(host, PackPhase::Knowledge, PhaseStatus::Running, 65, "importing knowledge");
        let bytes = std::fs::read(pack_dir.join(&k.from))
            .map_err(|e| PackError::Host(format!("knowledge {}: {e}", k.from)))?;
        let unlock = match &k.unlock {
            UnlockSpec::Key { key } => PackUnlock::KeyHex(key.clone()),
            UnlockSpec::Password { password } => PackUnlock::Password(password.clone()),
        };
        let source = k.source_name.clone().unwrap_or_else(|| manifest.pack.name.clone());
        host.import_source(&source, &bytes, &unlock)?;
        applied.sources.push(source.clone());
        receipt.imported_sources.push(source);
    }
    Ok(())
}

fn artifact_phase(
    host: &dyn PackHost,
    manifest: &Manifest,
    pack_dir: &Path,
    receipt: &mut Receipt,
    applied: &mut Applied,
) -> PackResult<()> {
    if let Some(d) = &manifest.components.dashboard {
        emit(host, PackPhase::Artifact, PhaseStatus::Running, 80, "inserting dashboard");
        let files = read_dir_flat(&pack_dir.join(&d.files_from))?
            .into_iter()
            .map(|(rel, bytes)| (rel, String::from_utf8_lossy(&bytes).into_owned()))
            .collect();
        let spec = ArtifactSpec {
            artifact_id: d.artifact_id.clone(),
            content_type: d.content_type.clone(),
            entry: d.entry.clone(),
            files,
        };
        host.upsert_artifact(&spec)?;
        applied.artifacts.push(d.artifact_id.clone());
        receipt.artifacts.push(d.artifact_id.clone());
    }
    Ok(())
}

fn rollback(host: &dyn PackHost, applied: &Applied) {
    for id in applied.artifacts.iter().rev() {
        let _ = host.remove_artifact(id);
    }
    for s in applied.sources.iter().rev() {
        let _ = host.remove_source(s);
    }
    for p in applied.pages.iter().rev() {
        let _ = host.delete_page(p);
    }
    for f in applied.files.iter().rev() {
        let _ = host.remove_file(f);
    }
}

#[derive(Debug, Clone, Default)]
pub struct UninstallOpts {
    pub purge_data: bool,
    pub force: bool, // delete sha-mismatched (user-edited) files too
}

/// Reverse an install strictly from its receipt. Default keeps GBrain user
/// data (seeded pages); only `--purge-data` removes them.
pub fn uninstall(host: &dyn PackHost, pack: &str, opts: &UninstallOpts) -> PackResult<()> {
    let receipt = host
        .read_receipt(pack)?
        .ok_or_else(|| PackError::NotInstalled(pack.to_string()))?;

    // Phase 3: files (sha-guarded).
    for f in &receipt.files {
        match std::fs::read(&f.path) {
            Ok(bytes) if Receipt::sha256_hex(&bytes) != f.sha256 && !opts.force => {
                emit(
                    host,
                    PackPhase::Place,
                    PhaseStatus::Failed,
                    0,
                    &format!("skip (user-modified): {}", f.path.display()),
                );
                continue;
            }
            _ => {}
        }
        host.remove_file(&f.path)?;
    }

    // Phase 4: artifacts.
    for id in &receipt.artifacts {
        host.remove_artifact(id)?;
    }

    // Phase 5: knowledge sources (ReadOnly, clean).
    for s in &receipt.imported_sources {
        host.remove_source(s)?;
    }

    // Phase 6: deactivate.
    let _ = host.reload_skills();

    // Phase 7: data — seed pages kept unless --purge-data.
    if opts.purge_data {
        for slug in &receipt.seeded_pages {
            host.delete_page(slug)?;
        }
    }

    // Phase 8: finalize.
    host.delete_receipt(pack)?;
    emit(host, PackPhase::Commit, PhaseStatus::Ok, 100, "uninstalled");
    Ok(())
}

/// Update an installed pack to a new version. Overwrites pack files, refreshes
/// the dashboard artifact and ReadOnly knowledge source, adds new seed pages
/// (only-if-absent), and never touches existing user data. Implemented as a
/// receipt-guarded uninstall-of-pack-owned-bits followed by a fresh install,
/// keeping the old receipt until the new one commits.
pub fn update(
    host: &dyn PackHost,
    manifest: &Manifest,
    raw_toml: &str,
    pack_dir: &Path,
    now_utc: &str,
) -> PackResult<Receipt> {
    let old = host
        .read_receipt(&manifest.pack.name)?
        .ok_or_else(|| PackError::NotInstalled(manifest.pack.name.clone()))?;

    // Remove only pack-owned bits (files, artifacts, sources) — NOT seed pages.
    for f in &old.files {
        let _ = host.remove_file(&f.path);
    }
    for id in &old.artifacts {
        let _ = host.remove_artifact(id);
    }
    for s in &old.imported_sources {
        let _ = host.remove_source(s);
    }

    // Fresh install (force=true so the same-version guard doesn't block). Seed
    // is only-if-absent, so existing user pages survive untouched.
    let opts = InstallOpts {
        force: true,
        now_utc: now_utc.to_string(),
        ..Default::default()
    };
    match install(host, manifest, raw_toml, pack_dir, &opts) {
        Ok(receipt) => Ok(receipt),
        Err(e) => {
            // The install failed and rolled back its OWN work, but we had already
            // removed the old pack's files/artifacts/sources above. Restoring the
            // old receipt now would make it reference files that no longer exist,
            // so delete it instead — leaving the pack cleanly uninstalled rather
            // than recorded-but-broken. Limitation: the previously installed
            // version's files are already gone; a fully transactional update
            // (stage-then-swap) is a follow-up.
            let _ = host.delete_receipt(&manifest.pack.name);
            Err(e)
        }
    }
}

/// Read a directory one level deep, returning (relative-name, bytes). Skills
/// and canvas-app bundles are flattened to a single level by the loader.
///
/// Symlinks are NEVER followed: a bundled symlink (e.g. `evil -> /etc/passwd`)
/// would otherwise let a pack exfiltrate files outside its own directory. We
/// use `entry.file_type()` (which does NOT traverse symlinks, unlike `is_file`/
/// `is_dir`) and skip any entry that is itself a symlink at every level.
fn read_dir_flat(root: &Path) -> PackResult<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(root)
        .map_err(|e| PackError::Host(format!("read_dir {}: {e}", root.display())))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = entry
            .file_type()
            .map_err(|e| PackError::Host(format!("file_type {}: {e}", path.display())))?;
        if ft.is_symlink() {
            continue; // never follow symlinks: a pack could point outside its dir
        }
        if ft.is_file() {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path)
                .map_err(|e| PackError::Host(format!("read {}: {e}", path.display())))?;
            out.push((name, bytes));
        } else if ft.is_dir() {
            // One nested level (e.g. skills/<name>/SKILL.md, conventions/*).
            let sub = path.file_name().unwrap().to_string_lossy().into_owned();
            for inner in std::fs::read_dir(&path).map_err(|e| PackError::Host(e.to_string()))?.flatten() {
                let ift = match inner.file_type() {
                    Ok(t) => t,
                    Err(e) => return Err(PackError::Host(e.to_string())),
                };
                if ift.is_symlink() {
                    continue; // skip nested symlinks too
                }
                if ift.is_file() {
                    let ip = inner.path();
                    let name = format!("{sub}/{}", ip.file_name().unwrap().to_string_lossy());
                    let bytes = std::fs::read(&ip).map_err(|e| PackError::Host(e.to_string()))?;
                    out.push((name, bytes));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic order
    Ok(out)
}
