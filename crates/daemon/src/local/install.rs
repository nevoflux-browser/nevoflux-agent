//! Downloading, extracting, verifying and atomically installing one pinned
//! engine release archive (`crate::local::release`) for a given
//! [`InstallKind`] (`crate::local::hardware`).
//!
//! [`install`] is the whole pipeline: a disk-space precheck, fetching every
//! archive [`crate::local::release::archive_for`] resolves (the main
//! platform/backend archive, plus a paired Windows CUDA cudart archive when
//! there is one) via [`crate::local::release::mirror_sources`], extracting
//! them into a shared staging directory, checking that the sentinel files
//! for `kind` are present, hashing every extracted file into a manifest,
//! and only then swapping the staging directory into its final, addressable
//! location ([`install_dir`]) and writing a [`crate::local::marker::Marker`]
//! that records what was installed. Any failure along the way removes the
//! staging directory, so a caller never observes a half-installed engine.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::local::hardware::{Backend, InstallKind};
use crate::local::marker::{self, FileEntry, Marker};
use crate::local::memory::MIB;
use crate::local::release::{self, EngineAsset, EngineRelease};
use crate::local::state::LocalError;
use crate::models::fetch::{self, FetchError};

/// Where installed engines live: `NEVOFLUX_LOCAL_CACHE_DIR/engine` when the
/// override is set (tests and Task 5.2's `#[ignore]` e2e test redirect both
/// this and `crate::models::models_dir()`'s sibling with the SAME
/// variable -- see that function's own doc comment for why they are not
/// currently wired together), else `dirs::cache_dir()/nevoflux/engine`,
/// matching the override-then-`cache_dir()` shape already used by
/// `crate::tts::asr::whisper`.
pub fn engine_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("NEVOFLUX_LOCAL_CACHE_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("engine"));
        }
    }
    dirs::cache_dir().map(|d| d.join("nevoflux").join("engine"))
}

/// The directory one specific (release tag, install kind) installs into:
/// `<tag>-<platform>-<backend>[-<variant>]`. Deterministic and collision-free
/// across every kind a single pinned release ships (see
/// `local::release::tests::every_hardware_probe_install_kind_resolves_to_a_pinned_archive`
/// for why `(platform, backend, variant)` alone already uniquely identifies
/// an archive).
pub fn install_dir(root: &Path, tag: &str, kind: &InstallKind) -> PathBuf {
    let mut name = format!("{tag}-{}-{}", kind.platform, backend_slug(kind.backend));
    if let Some(variant) = &kind.variant {
        name.push('-');
        name.push_str(variant);
    }
    root.join(name)
}

fn backend_slug(backend: Backend) -> &'static str {
    match backend {
        Backend::Cpu => "cpu",
        Backend::Vulkan => "vulkan",
        Backend::Cuda => "cuda",
        Backend::Metal => "metal",
    }
}

/// Bytes required in `root`'s filesystem to install: every archive's own
/// size, plus room to hold their extracted contents at once (archives are
/// not deleted until after extraction succeeds -- 2.5x their combined size
/// is a generous but not exact allowance for compression ratio), plus the
/// model that will be downloaded next, plus a fixed safety floor so a
/// "fits" verdict does not leave the disk completely full.
pub fn required_bytes(archives: &[u64], model_bytes: u64) -> u64 {
    let sum: u64 = archives.iter().fold(0u64, |acc, &n| acc.saturating_add(n));
    let extraction_room = sum.saturating_mul(5) / 2;
    sum.saturating_add(extraction_room)
        .saturating_add(model_bytes)
        .saturating_add(512 * MIB)
}

/// Free space on the filesystem containing `path`.
pub fn available_bytes(path: &Path) -> std::io::Result<u64> {
    fs2::available_space(path)
}

/// Progress reported by [`install`] as it moves through its phases.
/// `Downloading` may fire many times per archive (once per received chunk);
/// `Extracting` and `Verifying` each fire once, marking the start of that
/// phase across every archive in this install.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum InstallProgress {
    Downloading { done: u64, total: u64 },
    Extracting,
    Verifying,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Downloads, extracts, verifies and installs the archive(s)
/// `release::archive_for(rel, kind)` resolves, into a subdirectory of
/// `root`. Returns the final install directory on success.
///
/// Steps: disk precheck -> fetch every archive into `root/.staging/<name>`
/// (trying `release::mirror_sources` in order) -> extract each into a
/// shared `root/.staging/<dir>.tmp/` (a Windows CUDA app archive and its
/// cudart archive merge into the SAME tmp dir, since the cudart DLLs need
/// to sit alongside `llama-server.exe`) -> check `sentinels_ok` -> hash
/// every extracted file into a manifest -> atomically rename the tmp dir
/// into its final [`install_dir`] location -> write the
/// [`crate::local::marker::Marker`] -> delete the staged archive files.
/// Any failure from extraction onward removes the shared tmp dir, which
/// covers both archives in a Windows CUDA group since they share it.
pub async fn install(
    rel: &EngineRelease,
    kind: &InstallKind,
    root: &Path,
    cancel: &CancellationToken,
    on_progress: &mut (dyn FnMut(InstallProgress) + Send),
) -> Result<PathBuf, LocalError> {
    let (archive, cudart) =
        release::archive_for(rel, kind).ok_or_else(|| LocalError::DownloadFailed {
            detail: format!("no pinned archive for install kind {kind:?}"),
        })?;

    tokio::fs::create_dir_all(root)
        .await
        .map_err(|e| LocalError::DownloadFailed {
            detail: format!("{}: {e}", root.display()),
        })?;

    let archive_bytes: Vec<u64> = std::iter::once(archive.asset.bytes)
        .chain(cudart.map(|c| c.asset.bytes))
        .collect();
    let needed = required_bytes(&archive_bytes, 0);
    let available = available_bytes(root).map_err(|e| LocalError::DownloadFailed {
        detail: format!("checking free space on {}: {e}", root.display()),
    })?;
    if available < needed {
        return Err(LocalError::NoSpace { needed, available });
    }

    let staging = root.join(".staging");
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(|e| LocalError::DownloadFailed {
            detail: format!("{}: {e}", staging.display()),
        })?;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .user_agent("nevoflux-agent")
        .build()
        .map_err(|e| LocalError::DownloadFailed {
            detail: format!("http client: {e}"),
        })?;

    let mut staged_paths = Vec::new();
    for asset in std::iter::once(&archive.asset).chain(cudart.map(|c| &c.asset)) {
        let dest = staging.join(asset.name);
        let mut progress = |done: u64, total: u64| {
            on_progress(InstallProgress::Downloading { done, total });
        };
        fetch_asset(&client, rel.tag, asset, &dest, cancel, &mut progress).await?;
        staged_paths.push(dest);
    }

    let final_dir = install_dir(root, rel.tag, kind);
    let dir_name = final_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "engine".to_string());
    let tmp_dir = staging.join(format!("{dir_name}.tmp"));
    // Clear any leftover from a previous crashed attempt before we start --
    // extraction below merges every archive into this one directory, so a
    // stale partial extraction here would silently mix with fresh files.
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

    on_progress(InstallProgress::Extracting);
    for staged in &staged_paths {
        if let Err(detail) = extract_archive(staged, &tmp_dir) {
            tracing::warn!(archive = %staged.display(), %detail, "engine archive failed to extract");
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            return Err(LocalError::ArchiveCorrupt);
        }
    }

    on_progress(InstallProgress::Verifying);
    if let Err(missing) = sentinels_ok(&tmp_dir, kind) {
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        return Err(LocalError::SentinelMissing { missing });
    }

    let manifest_dir = tmp_dir.clone();
    let files = match tokio::task::spawn_blocking(move || build_manifest(&manifest_dir)).await {
        Ok(Ok(files)) => files,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "engine manifest build failed");
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            return Err(LocalError::ArchiveCorrupt);
        }
        Err(e) => {
            tracing::warn!(error = %e, "engine manifest build task panicked");
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            return Err(LocalError::ArchiveCorrupt);
        }
    };

    if let Ok(true) = tokio::fs::try_exists(&final_dir).await {
        if marker::read_marker(&final_dir).is_none() {
            // No marker -- a stale or foreign directory occupying our
            // target path, not a completed install. Safe to clear before
            // the atomic swap.
            let _ = tokio::fs::remove_dir_all(&final_dir).await;
        }
        // A directory WITH a marker already there is a previously completed
        // install at this exact (tag, kind). Leaving it alone rather than
        // deleting it protects against clobbering files a running engine
        // process may still have open; the rename below is left to fail
        // naturally in that case.
    }

    tokio::fs::rename(&tmp_dir, &final_dir).await.map_err(|e| {
        tracing::warn!(error = %e, from = %tmp_dir.display(), to = %final_dir.display(), "engine install could not be finalized");
        LocalError::ArchiveCorrupt
    })?;

    let now = now_unix();
    let installed_marker = Marker {
        tag: rel.tag.to_string(),
        kind: kind.clone(),
        archive_sha256: std::iter::once(archive.asset.sha256.to_string())
            .chain(cudart.map(|c| c.asset.sha256.to_string()))
            .collect(),
        files,
        installed_at: now,
        last_used_at: now,
        bad: None,
    };
    marker::write_marker(&final_dir, &installed_marker).map_err(|e| {
        tracing::warn!(error = %e, "engine marker write failed");
        LocalError::ArchiveCorrupt
    })?;

    for staged in &staged_paths {
        let _ = tokio::fs::remove_file(staged).await;
    }

    Ok(final_dir)
}

/// Fetches one asset, trying `release::mirror_sources` in order.
///
/// When `NEVOFLUX_ENGINE_MIRROR_BASE` is set (tests/dev only --
/// `release::mirror_sources`'s own doc comment), it replaces only the FIRST
/// source; the other two remain the real mirror-repo GitHub release and the
/// original fork/upstream URL. Falling back to those from a test or dev
/// override would defeat the entire point of the override -- an offline
/// test reaching for real GitHub the moment its local fixture server
/// returns anything other than success -- so this function stops after the
/// first source whenever the override is active, and tries all three only
/// in production (override unset).
async fn fetch_asset(
    client: &reqwest::Client,
    tag: &str,
    asset: &EngineAsset,
    dest: &Path,
    cancel: &CancellationToken,
    on_progress: &mut (dyn FnMut(u64, u64) + Send),
) -> Result<(), LocalError> {
    let sources = release::mirror_sources(tag, asset.name);
    let mirror_override_active = std::env::var("NEVOFLUX_ENGINE_MIRROR_BASE").is_ok();
    let try_sources: &[String] = if mirror_override_active {
        &sources[..1]
    } else {
        &sources[..]
    };

    let mut last_err: Option<FetchError> = None;
    for url in try_sources {
        match fetch::fetch_to(
            client,
            url,
            dest,
            asset.bytes,
            asset.sha256,
            cancel,
            on_progress,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retry = e.worth_another_source();
                tracing::warn!(url = %url, error = %e, "engine asset source failed");
                last_err = Some(e);
                if !retry {
                    break;
                }
            }
        }
    }

    Err(match last_err {
        Some(FetchError::Digest { .. }) => LocalError::ChecksumMismatch,
        Some(e) => LocalError::DownloadFailed {
            detail: e.to_string(),
        },
        None => LocalError::DownloadFailed {
            detail: "no source available".to_string(),
        },
    })
}

/// Resolves `entry_path`'s components into a destination-relative path,
/// refusing anything that could escape it. Whitelist, not blacklist (same
/// rule `crate::http::artifacts::safe_join` and `crate::profile::archive`'s
/// `confine` already apply to this codebase's other archive/path-join
/// sites): only `Normal` and `CurDir` components are accepted, so a `..`,
/// an absolute root, or (on Windows) a drive-letter prefix is rejected
/// outright rather than pattern-matched away.
fn confined_relative_path(entry_path: &Path) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for component in entry_path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "archive entry escapes the destination: {}",
                    entry_path.display()
                ))
            }
        }
    }
    Ok(out)
}

/// The single top-level directory every entry in `rel_paths` is nested
/// under, if there is one -- so [`extract_archive`] can flatten it away
/// (every engine archive ships one wrapper directory, e.g. `llama-b1/`,
/// that nothing downstream wants). `None` when there is no single shared
/// top (so entries extract at their own paths unchanged), including the
/// degenerate case where "the shared top" would swallow every entry with
/// nothing left underneath it -- that is not a wrapper directory, it is
/// the payload itself.
fn common_top_dir(rel_paths: &[PathBuf]) -> Option<PathBuf> {
    let first = rel_paths.first()?;
    let Component::Normal(top_os) = first.components().next()? else {
        return None;
    };
    for p in rel_paths {
        match p.components().next() {
            Some(Component::Normal(part)) if part == top_os => {}
            _ => return None,
        }
    }
    let top = PathBuf::from(top_os);
    let any_nested = rel_paths.iter().any(|p| {
        p.strip_prefix(&top)
            .map(|s| !s.as_os_str().is_empty())
            .unwrap_or(false)
    });
    any_nested.then_some(top)
}

/// Strips `top` (if any) from `rel`, returning `None` only if `rel` somehow
/// does not start with `top` -- which [`common_top_dir`] already guarantees
/// cannot happen for any path it was computed from, so this is a defensive
/// fallback rather than an expected path. An entry that strips down to the
/// empty path is the wrapper directory entry itself, not real content.
fn flatten(rel: &Path, top: &Option<PathBuf>) -> Option<PathBuf> {
    match top {
        Some(prefix) => rel.strip_prefix(prefix).ok().map(|p| p.to_path_buf()),
        None => Some(rel.to_path_buf()),
    }
}

/// Extracts `archive` (a `.zip` or `.tar.gz`, by extension) into `dest`,
/// flattening a single shared top-level wrapper directory if the archive
/// has one. Confines every entry to `dest` (see [`confined_relative_path`])
/// and preserves unix executable-bit permissions on unix.
pub fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    let ext = archive
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "zip" => extract_zip(archive, dest),
        "gz" => extract_tar_gz(archive, dest),
        other => Err(format!(
            "{}: unrecognized archive extension {other:?} (expected .zip or .tar.gz)",
            archive.display()
        )),
    }
}

fn extract_zip(archive_path: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("{}: {e}", archive_path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| format!("{}: not a zip archive: {e}", archive_path.display()))?;

    let mut rel_paths = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        rel_paths.push(confined_relative_path(Path::new(entry.name()))?);
    }
    let top = common_top_dir(&rel_paths);

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        let Some(rel) = flatten(&rel_paths[i], &top) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out_path = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("{}: {e}", out_path.display()))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let mut out_file =
            std::fs::File::create(&out_path).map_err(|e| format!("{}: {e}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out_file)
            .map_err(|e| format!("{}: {e}", out_path.display()))?;
        drop(out_file);

        #[cfg(unix)]
        {
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| format!("{}: {e}", out_path.display()))?;
            }
        }
    }
    Ok(())
}

fn extract_tar_gz(archive_path: &Path, dest: &Path) -> Result<(), String> {
    // First pass: collect confined relative paths to detect a shared
    // top-level wrapper directory. `tar` is a streaming format (no random
    // access like zip), so this re-decodes the same file from the start
    // rather than rewinding a shared reader.
    let rel_paths = {
        let file = std::fs::File::open(archive_path)
            .map_err(|e| format!("{}: {e}", archive_path.display()))?;
        let gz = flate2::read::GzDecoder::new(file);
        let mut ar = tar::Archive::new(gz);
        let mut paths = Vec::new();
        for entry in ar
            .entries()
            .map_err(|e| format!("{}: {e}", archive_path.display()))?
        {
            let entry = entry.map_err(|e| format!("{}: {e}", archive_path.display()))?;
            let raw = entry
                .path()
                .map_err(|e| format!("tar entry path: {e}"))?
                .to_path_buf();
            paths.push(confined_relative_path(&raw)?);
        }
        paths
    };
    let top = common_top_dir(&rel_paths);

    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("{}: {e}", archive_path.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut ar = tar::Archive::new(gz);
    let mut idx = 0usize;
    for entry in ar
        .entries()
        .map_err(|e| format!("{}: {e}", archive_path.display()))?
    {
        let mut entry = entry.map_err(|e| format!("{}: {e}", archive_path.display()))?;
        let rel_owned = rel_paths[idx].clone();
        idx += 1;
        let Some(rel) = flatten(&rel_owned, &top) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }

        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            // Whitelist, not blacklist (same rule as
            // `crate::profile::archive::unpack`): a symlink is how a
            // confined extraction becomes an arbitrary write, and no
            // engine archive needs one.
            continue;
        }

        let out_path = dest.join(&rel);
        if kind.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("{}: {e}", out_path.display()))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let mut out_file =
            std::fs::File::create(&out_path).map_err(|e| format!("{}: {e}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out_file)
            .map_err(|e| format!("{}: {e}", out_path.display()))?;
        drop(out_file);

        #[cfg(unix)]
        {
            if let Ok(mode) = entry.header().mode() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| format!("{}: {e}", out_path.display()))?;
            }
        }
    }
    Ok(())
}

fn check_present(dir: &Path, name: &str) -> Result<(), String> {
    if dir.join(name).is_file() {
        Ok(())
    } else {
        Err(format!("missing {name}"))
    }
}

/// Checks that the files an installed `kind` needs to actually run are
/// present in `dir`. The engine binary itself, always; on a Windows CUDA
/// install, additionally the cublas/cublasLt DLLs for the archive's own
/// CUDA major version (12 or 13, from the `cuda12*`/`cuda13*` variant
/// prefix -- the same derivation `release::archive_for` uses for cudart
/// pairing). Linux CUDA needs no extra sentinel: those archives link
/// against the system's own libcudart rather than bundling one (see
/// `hardware::select_cuda_kind`'s doc comment), which is exactly what
/// `hardware::probe`'s `cuda_runtime_lines` check already gates on before
/// this kind is ever selected.
pub fn sentinels_ok(dir: &Path, kind: &InstallKind) -> Result<(), String> {
    let binary = if kind.platform.starts_with("windows") {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    check_present(dir, binary)?;

    if kind.backend == Backend::Cuda && kind.platform.starts_with("windows") {
        let variant = kind.variant.as_deref().unwrap_or("");
        let major = if variant.starts_with("cuda13") {
            "13"
        } else {
            "12"
        };
        check_present(dir, &format!("cublas64_{major}.dll"))?;
        check_present(dir, &format!("cublasLt64_{major}.dll"))?;
    }

    Ok(())
}

fn sha256_file_sync(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Walks `dir` recursively and hashes every regular file into a
/// [`FileEntry`], sorted by path for a deterministic manifest. Synchronous
/// and potentially slow (whole-file hashing over however many files an
/// engine archive unpacks to) -- [`install`] runs this via
/// `tokio::task::spawn_blocking` rather than on the async executor.
pub fn build_manifest(dir: &Path) -> std::io::Result<Vec<FileEntry>> {
    let mut files = Vec::new();
    walk_manifest(dir, dir, &mut files)?;
    files.sort_by(|a: &FileEntry, b: &FileEntry| a.path.cmp(&b.path));
    Ok(files)
}

fn walk_manifest(root: &Path, current: &Path, out: &mut Vec<FileEntry>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_manifest(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let size = entry.metadata()?.len();
            let sha256 = sha256_file_sync(&path)?;
            out.push(FileEntry {
                path: rel_str,
                size,
                sha256,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    fn leak_str(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    fn leak_archives(v: Vec<release::EngineArchive>) -> &'static [release::EngineArchive] {
        Box::leak(v.into_boxed_slice())
    }

    // --- required_bytes ------------------------------------------------

    #[test]
    fn required_bytes_matches_the_documented_formula() {
        assert_eq!(required_bytes(&[100], 1000), 100 + 250 + 1000 + 512 * MIB);
    }

    #[test]
    fn required_bytes_sums_multiple_archives() {
        assert_eq!(
            required_bytes(&[100, 200], 0),
            300 + (300 * 5 / 2) + 512 * MIB
        );
    }

    // --- engine_root / install_dir --------------------------------------

    #[test]
    fn engine_root_honours_the_cache_dir_override() {
        let _guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NEVOFLUX_LOCAL_CACHE_DIR", tmp.path());
        let root = engine_root().unwrap();
        std::env::remove_var("NEVOFLUX_LOCAL_CACHE_DIR");
        assert_eq!(root, tmp.path().join("engine"));
    }

    #[test]
    fn install_dir_names_include_tag_platform_backend_and_variant() {
        let root = Path::new("/root");
        let kind = InstallKind {
            platform: "windows-x64".to_string(),
            backend: Backend::Cuda,
            variant: Some("cuda13-older".to_string()),
            cudart: Some("13.3".to_string()),
        };
        assert_eq!(
            install_dir(root, "b10909-mix-bea84f7", &kind),
            root.join("b10909-mix-bea84f7-windows-x64-cuda-cuda13-older")
        );

        let cpu_kind = InstallKind {
            platform: "linux-x64".to_string(),
            backend: Backend::Cpu,
            variant: None,
            cudart: None,
        };
        assert_eq!(
            install_dir(root, "b10909-mix-bea84f7", &cpu_kind),
            root.join("b10909-mix-bea84f7-linux-x64-cpu")
        );
    }

    // --- sentinels_ok ----------------------------------------------------

    fn cuda_kind() -> InstallKind {
        InstallKind {
            platform: "windows-x64".to_string(),
            backend: Backend::Cuda,
            variant: Some("cuda13-older".to_string()),
            cudart: Some("13.3".to_string()),
        }
    }

    #[test]
    fn sentinels_ok_fails_for_a_windows_cuda_dir_missing_cublaslt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server.exe"), b"x").unwrap();
        std::fs::write(dir.path().join("cublas64_13.dll"), b"x").unwrap();
        // cublasLt64_13.dll is missing.
        let err = sentinels_ok(dir.path(), &cuda_kind()).unwrap_err();
        assert!(err.contains("cublasLt64_13.dll"), "{err}");
    }

    #[test]
    fn sentinels_ok_passes_when_every_windows_cuda_file_is_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server.exe"), b"x").unwrap();
        std::fs::write(dir.path().join("cublas64_13.dll"), b"x").unwrap();
        std::fs::write(dir.path().join("cublasLt64_13.dll"), b"x").unwrap();
        assert!(sentinels_ok(dir.path(), &cuda_kind()).is_ok());
    }

    #[test]
    fn sentinels_ok_only_needs_the_binary_on_cpu() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server"), b"x").unwrap();
        let kind = InstallKind {
            platform: "linux-x64".to_string(),
            backend: Backend::Cpu,
            variant: None,
            cudart: None,
        };
        assert!(sentinels_ok(dir.path(), &kind).is_ok());
    }

    // --- extract_archive: zip --------------------------------------------

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            for (name, data) in entries {
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn zip_extraction_flattens_a_single_top_level_dir() {
        let bytes = build_zip(&[
            ("llama-b1/llama-server", b"binary"),
            ("llama-b1/README", b"hi"),
        ]);
        let src = tempfile::NamedTempFile::with_suffix(".zip").unwrap();
        std::fs::write(src.path(), &bytes).unwrap();
        let dest = tempfile::tempdir().unwrap();

        extract_archive(src.path(), dest.path()).unwrap();

        assert!(
            dest.path().join("llama-server").is_file(),
            "top-level llama-b1/ must be flattened away"
        );
        assert!(dest.path().join("README").is_file());
        assert!(!dest.path().join("llama-b1").exists());
    }

    #[test]
    fn zip_extraction_rejects_a_parent_dir_escape_and_writes_nothing() {
        let bytes = build_zip(&[("llama-b1/llama-server", b"binary"), ("../evil", b"pwned")]);
        let src = tempfile::NamedTempFile::with_suffix(".zip").unwrap();
        std::fs::write(src.path(), &bytes).unwrap();
        let outer = tempfile::tempdir().unwrap();
        let dest = outer.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        let result = extract_archive(src.path(), &dest);

        assert!(
            result.is_err(),
            "an escaping entry must reject the whole archive"
        );
        assert!(
            !outer.path().join("evil").exists(),
            "the escape must not have landed outside dest"
        );
        assert!(
            std::fs::read_dir(&dest).unwrap().next().is_none(),
            "nothing may be written when any entry is rejected"
        );
    }

    // --- extract_archive: tar.gz ------------------------------------------

    fn build_tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        use std::io::Write as _;
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            for (path, data, mode) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                builder.append_data(&mut header, path, *data).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_buf).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn tar_gz_extraction_flattens_the_top_dir() {
        let bytes = build_tar_gz(&[
            ("llama-b1/llama-server", b"binary", 0o755),
            ("llama-b1/README", b"hi", 0o644),
        ]);
        let src = tempfile::NamedTempFile::with_suffix(".tar.gz").unwrap();
        // NamedTempFile's suffix only affects the last extension component
        // (`.gz`), which is all `extract_archive` inspects.
        std::fs::write(src.path(), &bytes).unwrap();
        let dest = tempfile::tempdir().unwrap();

        extract_archive(src.path(), dest.path()).unwrap();

        assert!(dest.path().join("llama-server").is_file());
        assert!(dest.path().join("README").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn tar_gz_extraction_preserves_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let bytes = build_tar_gz(&[("llama-b1/llama-server", b"binary", 0o755)]);
        let src = tempfile::NamedTempFile::with_suffix(".tar.gz").unwrap();
        std::fs::write(src.path(), &bytes).unwrap();
        let dest = tempfile::tempdir().unwrap();

        extract_archive(src.path(), dest.path()).unwrap();

        let mode = std::fs::metadata(dest.path().join("llama-server"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    // --- build_manifest ----------------------------------------------------

    #[test]
    fn build_manifest_hashes_every_file_with_forward_slash_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server"), b"binary").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("lib.dll"), b"lib").unwrap();

        let files = build_manifest(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        let by_path: HashMap<_, _> = files.iter().map(|f| (f.path.clone(), f)).collect();
        let server = by_path.get("llama-server").expect("llama-server present");
        assert_eq!(server.size, 6);
        assert_eq!(server.sha256, sha256_hex(b"binary"));
        let lib = by_path
            .get("sub/lib.dll")
            .expect("nested file present with / separator");
        assert_eq!(lib.sha256, sha256_hex(b"lib"));
    }

    // --- end-to-end install() ----------------------------------------------

    struct MirrorBaseGuard;
    impl Drop for MirrorBaseGuard {
        fn drop(&mut self) {
            std::env::remove_var("NEVOFLUX_ENGINE_MIRROR_BASE");
        }
    }

    async fn spawn_fake_mirror(files: HashMap<String, Vec<u8>>) -> String {
        use axum::extract::{Path as AxumPath, State};
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::get;
        use axum::Router;

        async fn serve(
            State(files): State<Arc<HashMap<String, Vec<u8>>>>,
            AxumPath((_tag, asset)): AxumPath<(String, String)>,
        ) -> impl IntoResponse {
            match files.get(&asset) {
                Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        }

        let state = Arc::new(files);
        let app = Router::new()
            .route("/:tag/:asset", get(serve))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn dummy_sha256() -> &'static str {
        leak_str("0".repeat(64))
    }

    #[tokio::test]
    async fn install_produces_a_directory_with_a_verified_manifest_and_marker() {
        // Holds the SAME mutex `local::release`'s own tests use to guard
        // `NEVOFLUX_ENGINE_MIRROR_BASE` (`crate::llm_gateway::tests::ENV_MUTEX`)
        // across the whole async body, not just the `set_var` call -- a
        // concurrently-running test on another thread mutating the same
        // process-global env var mid-install would otherwise leak in.
        let _guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let zip_bytes = build_zip(&[("pkg/llama-server.exe", b"fake-engine-binary")]);
        let sha = sha256_hex(&zip_bytes);
        let mut files = HashMap::new();
        files.insert("test-cpu-ok.zip".to_string(), zip_bytes.clone());
        let base_url = spawn_fake_mirror(files).await;
        std::env::set_var("NEVOFLUX_ENGINE_MIRROR_BASE", &base_url);
        let _restore = MirrorBaseGuard;

        let archive = release::EngineArchive {
            platform: "windows-x64",
            backend: Backend::Cpu,
            variant: None,
            asset: EngineAsset {
                name: "test-cpu-ok.zip",
                bytes: zip_bytes.len() as u64,
                sha256: leak_str(sha),
            },
        };
        let rel = EngineRelease {
            tag: "test-tag-ok",
            upstream_tag: "test-tag-ok",
            archives: leak_archives(vec![archive]),
            cudart: &[],
            source: EngineAsset {
                name: "src",
                bytes: 0,
                sha256: dummy_sha256(),
            },
        };

        let root_dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let kind = InstallKind {
            platform: "windows-x64".to_string(),
            backend: Backend::Cpu,
            variant: None,
            cudart: None,
        };
        let mut phases = Vec::new();
        let final_dir = install(&rel, &kind, root_dir.path(), &cancel, &mut |p| {
            phases.push(p)
        })
        .await
        .expect("install should succeed");

        assert!(
            final_dir.join("llama-server.exe").is_file(),
            "the pkg/ top-level dir must be flattened away"
        );
        let m = marker::read_marker(&final_dir).expect("marker.json must exist");
        assert_eq!(m.tag, "test-tag-ok");
        assert_eq!(m.archive_sha256.len(), 1);
        for f in &m.files {
            let bytes = std::fs::read(final_dir.join(&f.path)).unwrap();
            assert_eq!(bytes.len() as u64, f.size);
            assert_eq!(
                sha256_hex(&bytes),
                f.sha256,
                "manifest hash must match the real file"
            );
        }
        assert!(phases
            .iter()
            .any(|p| matches!(p, InstallProgress::Downloading { .. })));
        assert!(phases
            .iter()
            .any(|p| matches!(p, InstallProgress::Extracting)));
        assert!(phases
            .iter()
            .any(|p| matches!(p, InstallProgress::Verifying)));
    }

    #[tokio::test]
    async fn install_leaves_no_final_dir_when_the_pinned_sha256_is_wrong() {
        let _guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let zip_bytes = build_zip(&[("pkg/llama-server.exe", b"fake-engine-binary")]);
        let mut files = HashMap::new();
        files.insert("test-cpu-bad.zip".to_string(), zip_bytes.clone());
        let base_url = spawn_fake_mirror(files).await;
        std::env::set_var("NEVOFLUX_ENGINE_MIRROR_BASE", &base_url);
        let _restore = MirrorBaseGuard;

        let archive = release::EngineArchive {
            platform: "windows-x64",
            backend: Backend::Cpu,
            variant: None,
            asset: EngineAsset {
                name: "test-cpu-bad.zip",
                bytes: zip_bytes.len() as u64,
                // Deliberately wrong: does not match the real bytes served
                // above, so `fetch_to`'s own digest check must fail this.
                sha256: leak_str("f".repeat(64)),
            },
        };
        let rel = EngineRelease {
            tag: "test-tag-bad",
            upstream_tag: "test-tag-bad",
            archives: leak_archives(vec![archive]),
            cudart: &[],
            source: EngineAsset {
                name: "src",
                bytes: 0,
                sha256: dummy_sha256(),
            },
        };

        let root_dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let kind = InstallKind {
            platform: "windows-x64".to_string(),
            backend: Backend::Cpu,
            variant: None,
            cudart: None,
        };
        let result = install(&rel, &kind, root_dir.path(), &cancel, &mut |_| {}).await;

        assert!(
            matches!(result, Err(LocalError::ChecksumMismatch)),
            "{result:?}"
        );
        let final_dir = install_dir(root_dir.path(), rel.tag, &kind);
        assert!(
            !final_dir.exists(),
            "no final dir may exist after a checksum mismatch"
        );
    }
}
