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
/// this and [`local_models_dir`], its sibling below, with the SAME
/// variable), else `dirs::cache_dir()/nevoflux/engine`, matching the
/// override-then-`cache_dir()` shape already used by
/// `crate::tts::asr::whisper`.
pub fn engine_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("NEVOFLUX_LOCAL_CACHE_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("engine"));
        }
    }
    dirs::cache_dir().map(|d| d.join("nevoflux").join("engine"))
}

/// Where downloaded model weights (GGUF files) live:
/// `NEVOFLUX_LOCAL_CACHE_DIR/models` when the override is set, else
/// `crate::models::models_dir()` — the SAME shared `$CACHE/nevoflux/models/`
/// directory `tts::asr`/`tts::kokoro` already use (v3 §6; ruling R53: this
/// does not touch `models::models_dir()` itself, since changing it would
/// reach into those unrelated speech features). Colocated with
/// [`engine_root`] (fix round 1, Minor 11) rather than living in
/// `local::rpc` — the module that actually consumes it — because both honor
/// the SAME `NEVOFLUX_LOCAL_CACHE_DIR` override and Task 5.2's end-to-end
/// test redirects both together; a reader chasing that variable should find
/// both accessors in one place.
pub fn local_models_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("NEVOFLUX_LOCAL_CACHE_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("models"));
        }
    }
    crate::models::models_dir()
}

/// The `<platform>-<backend>[-<variant>]` identity string for `kind` --
/// shared by [`install_dir`] (as the tail of the directory name) and
/// [`install`]'s `Marker.kind` (the whole string), so the two can never
/// drift apart. `crate::local::marker::read_marker` relies on that
/// equality to verify a marker matches the directory it was found in
/// (controller ruling on Task 2.4 review finding M4).
fn kind_suffix(kind: &InstallKind) -> String {
    let mut s = format!("{}-{}", kind.platform, backend_slug(kind.backend));
    if let Some(variant) = &kind.variant {
        s.push('-');
        s.push_str(variant);
    }
    s
}

/// The directory one specific (release tag, install kind) installs into:
/// `<tag>-<kind_suffix>`. Deterministic and collision-free across every
/// kind a single pinned release ships (see
/// `local::release::tests::every_hardware_probe_install_kind_resolves_to_a_pinned_archive`
/// for why `(platform, backend, variant)` alone already uniquely identifies
/// an archive).
pub fn install_dir(root: &Path, tag: &str, kind: &InstallKind) -> PathBuf {
    root.join(format!("{tag}-{}", kind_suffix(kind)))
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
/// `Downloading{done, total}` is aggregated across every archive in this
/// install (Task 2.4 review finding M3): `total` is the combined size of
/// every archive the group needs (the main platform/backend archive, plus
/// a Windows CUDA cudart archive when one is paired in), and `done` is
/// monotonically non-decreasing across the whole download phase -- a
/// per-archive reset back to zero partway through a Windows CUDA install
/// would make a progress bar driven off this jump backwards. `Extracting`
/// and `Verifying` each fire once, marking the start of that phase across
/// every archive in the group.
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
/// (trying `release::mirror_sources` in order; a source whose bytes fail
/// the pinned sha256 is deleted, never left on disk to be resumed forever
/// -- `models::fetch::fetch_to`'s own behavior, reused here rather than
/// reimplemented) -> extract each into a shared `root/.staging/<dir>.tmp/`
/// (a Windows CUDA app archive and its cudart archive merge into the SAME
/// tmp dir, since the cudart DLLs need to sit alongside `llama-server.exe`)
/// -> check `sentinels_ok` -> hash every extracted file into a manifest ->
/// atomically rename the tmp dir into its final [`install_dir`] location ->
/// write the [`crate::local::marker::Marker`] -> delete the staged archive
/// files. A target directory that already holds a *valid* marker for this
/// exact (tag, kind) is treated as an already-completed install (Task 2.4
/// review finding M2): the freshly built tmp dir is discarded and
/// `Ok(final_dir)` is returned, rather than attempting a rename onto a
/// non-empty destination that can only ever fail.
///
/// **Cleanup rule (Task 2.4 review finding M2(b), stated once so it does
/// not have to be re-derived from the arms):** every terminal `Err` return
/// in this function first calls [`cleanup_staging`] on whatever this
/// attempt has put under `.staging/` so far -- the shared extraction
/// directory (`tmp_dir`, or, once past the rename step, `final_dir` itself,
/// which by then holds the same not-yet-trusted content under a different
/// name) AND every archive staged so far in this attempt. This applies
/// uniformly, including failure points before any archive has downloaded
/// (where it is a harmless no-op) -- deliberately not selective, because
/// arm-by-arm reasoning is exactly what produced the original gap (the
/// rename-failure arm removed `tmp_dir` but not `staged_paths`, and the
/// mid-group download-failure arm removed neither). `.staging/` promises
/// no resume guarantee ACROSS `install()` attempts (v3 §6: `临时，可随时清空`)
/// -- deleting a verified archive on failure means a retry re-downloads it,
/// which is the correct trade-off here; a silent multi-hundred-MB leak is
/// worse. This is distinct from, and does not touch,
/// `models::fetch::fetch_to`'s own `.part` resume mechanism, which operates
/// WITHIN a single archive's download and stays intentional.
pub async fn install(
    rel: &EngineRelease,
    kind: &InstallKind,
    root: &Path,
    cancel: &CancellationToken,
    on_progress: &mut (dyn FnMut(InstallProgress) + Send),
) -> Result<PathBuf, LocalError> {
    // Computed up front (before anything can fail) so every arm below,
    // including the earliest ones, can uniformly call `cleanup_staging`.
    let final_dir = install_dir(root, rel.tag, kind);
    let dir_name = final_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "engine".to_string());
    let staging = root.join(".staging");
    let tmp_dir = staging.join(format!("{dir_name}.tmp"));
    let mut staged_paths: Vec<PathBuf> = Vec::new();

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

    // Aggregated across the whole group (Task 2.4 review finding M3): a
    // Windows CUDA install fetches two archives, and `done` must keep
    // climbing across both rather than resetting to zero at the second.
    let total_download_bytes: u64 = archive_bytes.iter().sum();
    let mut completed_before: u64 = 0;
    for (asset, asset_bytes) in std::iter::once(&archive.asset)
        .chain(cudart.map(|c| &c.asset))
        .zip(archive_bytes.iter().copied())
    {
        let dest = staging.join(asset.name);
        let mut progress = |done: u64, _total: u64| {
            on_progress(InstallProgress::Downloading {
                done: completed_before.saturating_add(done),
                total: total_download_bytes,
            });
        };
        if let Err(e) = fetch_asset(&client, rel.tag, asset, &dest, cancel, &mut progress).await {
            // Task 2.4 review finding M2(b): a Windows CUDA group's SECOND
            // archive failing must not leave the FIRST one's already-staged
            // (and sha-verified) file behind.
            cleanup_staging(&tmp_dir, &staged_paths).await;
            return Err(e);
        }
        completed_before = completed_before.saturating_add(asset_bytes);
        staged_paths.push(dest);
    }

    // Clear any leftover from a previous crashed attempt before we start --
    // extraction below merges every archive into this one directory, so a
    // stale partial extraction here would silently mix with fresh files.
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

    on_progress(InstallProgress::Extracting);
    for staged in &staged_paths {
        if let Err(detail) = extract_archive(staged, &tmp_dir) {
            tracing::warn!(archive = %staged.display(), %detail, "engine archive failed to extract");
            cleanup_staging(&tmp_dir, &staged_paths).await;
            return Err(LocalError::ArchiveCorrupt);
        }
    }

    on_progress(InstallProgress::Verifying);
    if let Err(missing) = sentinels_ok(&tmp_dir, kind) {
        cleanup_staging(&tmp_dir, &staged_paths).await;
        return Err(LocalError::SentinelMissing { missing });
    }

    let manifest_dir = tmp_dir.clone();
    let files = match tokio::task::spawn_blocking(move || build_manifest(&manifest_dir)).await {
        Ok(Ok(files)) => files,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "engine manifest build failed");
            cleanup_staging(&tmp_dir, &staged_paths).await;
            return Err(LocalError::ArchiveCorrupt);
        }
        Err(e) => {
            tracing::warn!(error = %e, "engine manifest build task panicked");
            cleanup_staging(&tmp_dir, &staged_paths).await;
            return Err(LocalError::ArchiveCorrupt);
        }
    };

    if let Ok(true) = tokio::fs::try_exists(&final_dir).await {
        if marker::read_marker(&final_dir).is_some() {
            // Already installed at this exact (tag, kind): the ordinary
            // case for a repeated install, not a failure (Task 2.4 review
            // finding M2). A rename onto this non-empty destination is
            // guaranteed to fail (Windows refuses it; Unix gives
            // ENOTEMPTY), so attempting it would turn a routine re-install
            // into a permanent `ArchiveCorrupt` dead stop with a false
            // diagnosis. The freshly built (and identical, sha-pinned) tmp
            // dir and staged archives are simply discarded (same cleanup
            // as a failure, even though this is a success outcome); the
            // existing install's marker and files are left untouched
            // rather than risk deleting files a running engine process may
            // still have open.
            cleanup_staging(&tmp_dir, &staged_paths).await;
            return Ok(final_dir);
        }
        // No marker -- a stale or foreign directory occupying our target
        // path, not a completed install. Safe to clear before the swap.
        let _ = tokio::fs::remove_dir_all(&final_dir).await;
    }

    if let Err(e) = tokio::fs::rename(&tmp_dir, &final_dir).await {
        tracing::warn!(error = %e, from = %tmp_dir.display(), to = %final_dir.display(), "engine install could not be finalized");
        cleanup_staging(&tmp_dir, &staged_paths).await;
        return Err(LocalError::ArchiveCorrupt);
    }

    let now = now_unix();
    let installed_marker = Marker {
        tag: rel.tag.to_string(),
        kind: kind_suffix(kind),
        archive_sha256: std::iter::once(archive.asset.sha256.to_string())
            .chain(cudart.map(|c| c.asset.sha256.to_string()))
            .collect(),
        files,
        installed_at: now,
        last_used_at: now,
        bad: None,
    };
    if let Err(e) = marker::write_marker(&final_dir, &installed_marker) {
        tracing::warn!(error = %e, "engine marker write failed");
        // `tmp_dir` no longer exists -- it was just renamed into
        // `final_dir`, which is therefore what needs scrubbing here
        // instead: a fully extracted but now permanently unmarked (hence
        // untrusted, per `read_marker`) tree is exactly the kind of
        // orphan this cleanup rule exists to prevent.
        cleanup_staging(&final_dir, &staged_paths).await;
        return Err(LocalError::ArchiveCorrupt);
    }

    for staged in &staged_paths {
        let _ = tokio::fs::remove_file(staged).await;
    }

    Ok(final_dir)
}

/// Removes everything one `install()` attempt may have put under
/// `.staging/`: `extra_dir` (`tmp_dir` for every arm but one -- see
/// [`install`]'s own doc comment for the one exception, the post-rename
/// marker-write failure, which passes `final_dir` instead) and every
/// archive in `staged_paths`. Every removal is best-effort (`let _ =`):
/// this runs on a failure path already, and a stray leftover file failing
/// to delete must not mask or replace the real error being returned.
async fn cleanup_staging(extra_dir: &Path, staged_paths: &[PathBuf]) {
    let _ = tokio::fs::remove_dir_all(extra_dir).await;
    for staged in staged_paths {
        let _ = tokio::fs::remove_file(staged).await;
    }
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
///
/// A source whose bytes fail the pinned sha256 is deleted by
/// `models::fetch::fetch_to` itself (never left on disk to be resumed
/// forever against the same wrong pin) before this function even sees the
/// error -- that deletion is not reimplemented here.
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
        // `rel_paths` was built in the same 0..zip.len() order just above
        // and zip's `by_index` is stable/random-access (unlike tar's
        // streaming reader, no re-decode happens between the two loops),
        // so `rel_paths[i]` is safe here -- see `extract_tar_gz` for the
        // corresponding guarded lookup where that guarantee does not hold.
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

        // Every pinned `.zip` asset in `local::release::ENGINE_PINNED` is
        // Windows-only (linux/macos archives are all `.tar.gz`), so this
        // branch is currently dead weight rather than a live risk -- kept
        // for correctness if that ever changes, and because `zip::ZipFile`
        // exposes the bit for free.
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
        // Task 2.4 review finding m5: the first pass decoded the same file
        // from a fresh reader, so counts match in every observed case, but
        // an out-of-range index must fail gracefully rather than panic --
        // this project has already shipped two parse/slice panics on
        // exactly this class of "should always match" assumption (GGUF
        // unvalidated `u64` length; `&text[..500]` char boundary).
        let Some(rel_owned) = rel_paths.get(idx).cloned() else {
            return Err(format!(
                "{}: tar entry {idx} has no matching path from the first pass ({} collected)",
                archive_path.display(),
                rel_paths.len()
            ));
        };
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

/// One row of v3 §7.4's sentinel table: which `(platform, backend)`
/// combinations it applies to (a glob pattern matched against
/// `"<platform>-<backend_slug>"`, e.g. `"linux-*-cpu"`,
/// `"windows-x64-cuda*"`, `"macos-*"` -- the row keys are patterns
/// themselves, not exact strings, so matching a kind to its row is a glob
/// match too), and the file patterns that row requires. A file pattern may
/// contain the literal placeholder `{X}`, substituted with the CUDA major
/// version derived from the kind's own `cuda12*`/`cuda13*` variant prefix
/// (the same derivation `release::archive_for` uses to pair an archive
/// with its cudart runtime) -- never hard-coded, so a `cuda12-*` install
/// correctly expects `cudart64_12.dll`, not `cudart64_13.dll`.
struct SentinelRow {
    platform_backend_pattern: &'static str,
    files: &'static [&'static str],
}

const SENTINEL_TABLE: &[SentinelRow] = &[
    SentinelRow {
        platform_backend_pattern: "linux-*-cpu",
        files: &["libllama.so*", "libggml-base.so*", "libggml-cpu*.so*"],
    },
    SentinelRow {
        platform_backend_pattern: "linux-*-vulkan",
        files: &["libllama.so*", "libggml-base.so*", "libggml-cpu*.so*"],
    },
    SentinelRow {
        platform_backend_pattern: "linux-*-cuda*",
        files: &[
            "libllama.so*",
            "libggml-base.so*",
            "libggml-cpu*.so*",
            "libggml-cuda.so",
        ],
    },
    SentinelRow {
        platform_backend_pattern: "windows-x64-cuda*",
        files: &[
            "cublas64_{X}.dll",
            "cublasLt64_{X}.dll",
            "llama.dll",
            "ggml-cuda.dll",
            "cudart64_{X}.dll",
        ],
    },
    SentinelRow {
        platform_backend_pattern: "macos-*",
        files: &["libllama*.dylib", "libggml*.dylib"],
    },
];

/// The CUDA major version (`"12"` or `"13"`) a Windows/Linux CUDA `kind`
/// was built against, derived from its variant's own `cuda12*`/`cuda13*`
/// prefix -- the same rule `release::archive_for` already uses to pair an
/// archive with its cudart runtime. `None` for a non-CUDA kind, or a CUDA
/// kind with no recognizable variant prefix (should not happen for any
/// kind `hardware::fallback_chain` can actually produce).
fn cuda_major(kind: &InstallKind) -> Option<&'static str> {
    if kind.backend != Backend::Cuda {
        return None;
    }
    let variant = kind.variant.as_deref().unwrap_or("");
    if variant.starts_with("cuda13") {
        Some("13")
    } else if variant.starts_with("cuda12") {
        Some("12")
    } else {
        None
    }
}

/// Checks that the files an installed `kind` needs to actually run are
/// present in `dir`, per v3 §7.4's sentinel table ([`SENTINEL_TABLE`]):
/// the engine binary always, plus every row whose pattern matches
/// `"<platform>-<backend_slug>"`. `check_present` glob-matches against the
/// directory's actual entries (not an exact-name `is_file` check), since
/// several of the table's own patterns need it (`libllama.so*`,
/// `libggml-cpu*.so*`, `libggml*.dylib`, `cudart64_{X}.dll` after `{X}`
/// substitution).
pub fn sentinels_ok(dir: &Path, kind: &InstallKind) -> Result<(), String> {
    let binary = if kind.platform.starts_with("windows") {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    check_present(dir, binary)?;

    let subject = format!("{}-{}", kind.platform, backend_slug(kind.backend));
    let major = cuda_major(kind);

    for row in SENTINEL_TABLE {
        let row_pattern = glob::Pattern::new(row.platform_backend_pattern).map_err(|e| {
            format!(
                "invalid sentinel row pattern {:?}: {e}",
                row.platform_backend_pattern
            )
        })?;
        if !row_pattern.matches(&subject) {
            continue;
        }
        for file_pattern in row.files {
            let resolved = match major {
                Some(x) => file_pattern.replace("{X}", x),
                None => file_pattern.to_string(),
            };
            check_present(dir, &resolved)?;
        }
    }

    Ok(())
}

/// Whether some entry directly inside `dir` matches glob `pattern` (e.g.
/// `"libllama.so*"`, or an exact name like `"llama-server.exe"`, which a
/// pattern with no wildcard characters matches literally).
fn check_present(dir: &Path, pattern: &str) -> Result<(), String> {
    let compiled = glob::Pattern::new(pattern)
        .map_err(|e| format!("invalid sentinel pattern {pattern:?}: {e}"))?;
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let found = entries.filter_map(|e| e.ok()).any(|e| {
        e.file_name()
            .to_str()
            .map(|name| compiled.matches(name))
            .unwrap_or(false)
    });
    if found {
        Ok(())
    } else {
        Err(format!("missing {pattern}"))
    }
}

/// Hashes one file's contents as sha256, streaming in fixed-size chunks
/// rather than reading it whole into memory (an engine archive's largest
/// extracted file can be several hundred MB). `pub(crate)` rather than
/// private: `crate::local::integrity::verify_manifest` (Task 2.5) reuses
/// this exact routine for its cold-start file-by-file re-hash against the
/// install marker's manifest, rather than duplicating it.
pub(crate) fn sha256_file_sync(path: &Path) -> std::io::Result<String> {
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

    /// Removes one process-global env var on drop, so a panic mid-test
    /// can't leak an override into every later test in the process (Task
    /// 2.4 review finding m7).
    struct EnvVarGuard(&'static str);
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
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
        let _restore = EnvVarGuard("NEVOFLUX_LOCAL_CACHE_DIR");
        let root = engine_root().unwrap();
        assert_eq!(root, tmp.path().join("engine"));
    }

    #[test]
    fn local_models_dir_honours_the_same_cache_dir_override() {
        let _guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NEVOFLUX_LOCAL_CACHE_DIR", tmp.path());
        let _restore = EnvVarGuard("NEVOFLUX_LOCAL_CACHE_DIR");
        let dir = local_models_dir().unwrap();
        assert_eq!(dir, tmp.path().join("models"));
    }

    #[test]
    fn local_models_dir_falls_back_to_the_shared_speech_models_dir_without_an_override() {
        let _guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("NEVOFLUX_LOCAL_CACHE_DIR");
        assert_eq!(local_models_dir(), crate::models::models_dir());
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

    /// Controller ruling on Task 2.4 review finding M4: `Marker.kind` is a
    /// string, lossless across `kind_suffix -> parse_kind_suffix ->
    /// InstallKind`, over every kind `hardware::fallback_chain` can
    /// actually produce (not a hand-picked list). `cudart` is not encoded
    /// in the string at all; it round-trips because the parse side
    /// re-derives it via `hardware::known_cudart_version(variant)`, the
    /// exact same rule `hardware::select_cuda_kind` used to set it in the
    /// first place.
    #[test]
    fn kind_suffix_round_trips_through_every_kind_the_hardware_guard_can_produce() {
        use crate::local::config::BackendPref;
        use crate::local::hardware::{fallback_chain, GpuInfo, HardwareProbe};
        use std::collections::HashSet;

        fn parse_kind_suffix(s: &str) -> Option<InstallKind> {
            const PLATFORMS: &[&str] = &[
                "windows-x64",
                "windows-arm64",
                "linux-x64",
                "linux-arm64",
                "macos-x64",
                "macos-arm64",
            ];
            let platform = *PLATFORMS
                .iter()
                .find(|p| s.starts_with(**p) && s[p.len()..].starts_with('-'))?;
            let rest = &s[platform.len() + 1..];

            const BACKENDS: &[(&str, Backend)] = &[
                ("cpu", Backend::Cpu),
                ("vulkan", Backend::Vulkan),
                ("cuda", Backend::Cuda),
                ("metal", Backend::Metal),
            ];
            let (slug, backend) = BACKENDS
                .iter()
                .find(|(slug, _)| rest == *slug || rest.starts_with(&format!("{slug}-")))?;
            let remainder = &rest[slug.len()..];
            let variant = if remainder.is_empty() {
                None
            } else {
                Some(remainder.trim_start_matches('-').to_string())
            };
            let cudart = if *backend == Backend::Cuda {
                variant
                    .as_deref()
                    .and_then(crate::local::hardware::known_cudart_version)
                    .map(str::to_string)
            } else {
                None
            };

            Some(InstallKind {
                platform: platform.to_string(),
                backend: *backend,
                variant,
                cudart,
            })
        }

        let gib = crate::local::memory::GIB;
        struct PlatformCase {
            os: &'static str,
            arch: &'static str,
        }
        let platforms = [
            PlatformCase {
                os: "linux",
                arch: "x86_64",
            },
            PlatformCase {
                os: "linux",
                arch: "aarch64",
            },
            PlatformCase {
                os: "windows",
                arch: "x86_64",
            },
            PlatformCase {
                os: "windows",
                arch: "aarch64",
            },
            PlatformCase {
                os: "macos",
                arch: "x86_64",
            },
            PlatformCase {
                os: "macos",
                arch: "aarch64",
            },
        ];
        let caps = [
            (6, 1),
            (7, 5),
            (8, 0),
            (8, 6),
            (8, 9),
            (9, 0),
            (10, 0),
            (12, 0),
            (8, 7),
            (7, 0),
        ];

        let mut kinds: HashSet<InstallKind> = HashSet::new();
        for pc in &platforms {
            let no_gpu = HardwareProbe {
                os: pc.os.to_string(),
                arch: pc.arch.to_string(),
                vulkan_available: true,
                ram_bytes: 16 * gib,
                ..Default::default()
            };
            for kind in fallback_chain(&no_gpu, BackendPref::Auto) {
                kinds.insert(kind);
            }
            for cap in caps {
                for driver_major in [12u32, 13u32] {
                    let cuda_runtime_lines = if pc.os == "linux" {
                        vec![12, 13]
                    } else {
                        vec![]
                    };
                    let p = HardwareProbe {
                        os: pc.os.to_string(),
                        arch: pc.arch.to_string(),
                        nvidia_gpus: vec![GpuInfo {
                            name: "test-gpu".to_string(),
                            vram_bytes: 8 * gib,
                            compute_cap: Some(cap),
                        }],
                        has_physical_nvidia: true,
                        has_usable_nvidia: true,
                        driver_cuda_version: Some((driver_major, 0)),
                        cuda_runtime_lines,
                        vulkan_available: true,
                        ram_bytes: 16 * gib,
                        macos_version: None,
                    };
                    for kind in fallback_chain(&p, BackendPref::Auto) {
                        kinds.insert(kind);
                    }
                }
            }
        }

        assert!(
            kinds.len() >= 20,
            "expected a broad set of kinds, got {}",
            kinds.len()
        );
        for kind in &kinds {
            let s = kind_suffix(kind);
            let parsed =
                parse_kind_suffix(&s).unwrap_or_else(|| panic!("failed to parse {s:?} back"));
            assert_eq!(&parsed, kind, "round trip mismatch for {s:?}");
        }
    }

    // --- sentinels_ok ----------------------------------------------------

    fn cuda_kind(platform: &str, variant: &str) -> InstallKind {
        InstallKind {
            platform: platform.to_string(),
            backend: Backend::Cuda,
            variant: Some(variant.to_string()),
            cudart: None,
        }
    }

    fn plain_kind(platform: &str, backend: Backend) -> InstallKind {
        InstallKind {
            platform: platform.to_string(),
            backend,
            variant: None,
            cudart: None,
        }
    }

    #[test]
    fn sentinels_ok_fails_for_a_windows_cuda_dir_missing_cublaslt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server.exe"), b"x").unwrap();
        std::fs::write(dir.path().join("cublas64_13.dll"), b"x").unwrap();
        std::fs::write(dir.path().join("llama.dll"), b"x").unwrap();
        std::fs::write(dir.path().join("ggml-cuda.dll"), b"x").unwrap();
        std::fs::write(dir.path().join("cudart64_13.dll"), b"x").unwrap();
        // cublasLt64_13.dll is missing.
        let err = sentinels_ok(dir.path(), &cuda_kind("windows-x64", "cuda13-older")).unwrap_err();
        assert!(err.contains("cublasLt64_13.dll"), "{err}");
    }

    #[test]
    fn sentinels_ok_passes_when_every_windows_cuda13_file_is_present() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "llama-server.exe",
            "cublas64_13.dll",
            "cublasLt64_13.dll",
            "llama.dll",
            "ggml-cuda.dll",
            "cudart64_13.dll",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        assert!(sentinels_ok(dir.path(), &cuda_kind("windows-x64", "cuda13-older")).is_ok());
    }

    /// Coordinator clarification: `{X}` must come from the kind's own
    /// variant, never hard-coded -- a cuda12 install expects the cuda12
    /// DLL names, not cuda13's.
    #[test]
    fn sentinels_ok_derives_the_cuda_major_from_the_variant_not_a_constant() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "llama-server.exe",
            "cublas64_12.dll",
            "cublasLt64_12.dll",
            "llama.dll",
            "ggml-cuda.dll",
            "cudart64_12.dll",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let kind = cuda_kind("windows-x64", "cuda12-legacy");
        assert!(
            sentinels_ok(dir.path(), &kind).is_ok(),
            "cuda12 files must satisfy a cuda12 kind"
        );

        // The cuda13-named files are NOT present, so a cuda13 kind must
        // fail against this same directory.
        let cuda13_kind = cuda_kind("windows-x64", "cuda13-older");
        assert!(sentinels_ok(dir.path(), &cuda13_kind).is_err());
    }

    #[test]
    fn sentinels_ok_requires_shared_libraries_on_linux_cpu_and_vulkan() {
        for backend in [Backend::Cpu, Backend::Vulkan] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("llama-server"), b"x").unwrap();
            let kind = plain_kind("linux-x64", backend);
            let err = sentinels_ok(dir.path(), &kind).unwrap_err();
            assert!(err.contains("libllama.so"), "{err}");

            std::fs::write(dir.path().join("libllama.so.1"), b"x").unwrap();
            std::fs::write(dir.path().join("libggml-base.so"), b"x").unwrap();
            std::fs::write(dir.path().join("libggml-cpu-avx2.so"), b"x").unwrap();
            assert!(sentinels_ok(dir.path(), &kind).is_ok(), "{backend:?}");
        }
    }

    #[test]
    fn sentinels_ok_requires_libggml_cuda_on_linux_cuda_in_addition_to_the_base_libs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server"), b"x").unwrap();
        std::fs::write(dir.path().join("libllama.so"), b"x").unwrap();
        std::fs::write(dir.path().join("libggml-base.so"), b"x").unwrap();
        std::fs::write(dir.path().join("libggml-cpu.so"), b"x").unwrap();
        let kind = cuda_kind("linux-x64", "cuda13-older");
        let err = sentinels_ok(dir.path(), &kind).unwrap_err();
        assert!(err.contains("libggml-cuda.so"), "{err}");

        std::fs::write(dir.path().join("libggml-cuda.so"), b"x").unwrap();
        assert!(sentinels_ok(dir.path(), &kind).is_ok());
    }

    #[test]
    fn sentinels_ok_requires_dylibs_on_macos() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server"), b"x").unwrap();
        let kind = plain_kind("macos-arm64", Backend::Metal);
        assert!(sentinels_ok(dir.path(), &kind).is_err());

        std::fs::write(dir.path().join("libllama.dylib"), b"x").unwrap();
        std::fs::write(dir.path().join("libggml-base.dylib"), b"x").unwrap();
        assert!(sentinels_ok(dir.path(), &kind).is_ok());
    }

    #[test]
    fn sentinels_ok_only_needs_the_binary_on_windows_cpu() {
        // windows-x64-cpu matches no row in the sentinel table (only
        // windows-x64-cuda* carries DLL requirements), so the binary alone
        // is sufficient -- unlike linux-x64-cpu, which now also requires
        // the shared-library row.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server.exe"), b"x").unwrap();
        let kind = plain_kind("windows-x64", Backend::Cpu);
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

    // Task 2.4 review finding m8: this test has never executed on this
    // Windows development machine (the daemon repo has no Linux/macOS CI
    // run yet for this branch) -- kept, not deleted, pending one. There is
    // no equivalent zip-side test because it would be vacuous: every
    // pinned `.zip` asset in `local::release::ENGINE_PINNED` is
    // Windows-only (all linux/macos assets are `.tar.gz`), so the zip
    // extractor's own `unix_mode()` handling in `extract_zip` never runs
    // against a real archive on a unix host either.
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

    /// Like [`spawn_fake_mirror`], but streams its (single, fixed) body
    /// back after a short delay, regardless of which asset was requested.
    /// Only used by the cancellation test: a pre-cancelled token needs a
    /// genuine window where `models::fetch::fetch_to`'s internal
    /// `tokio::select!` is still waiting on the network so cancellation
    /// can win deterministically -- without the delay, a small in-memory
    /// body can arrive in the very first poll, same as the
    /// already-resolved cancellation future, making the outcome a coin
    /// flip between the two ready branches.
    async fn spawn_delayed_fake_mirror(bytes: Vec<u8>) -> String {
        use axum::body::Body;
        use axum::response::IntoResponse;
        use axum::routing::get;
        use axum::Router;
        use futures::stream;

        let bytes = Arc::new(bytes);
        let app = Router::new().route(
            "/:tag/:asset",
            get(move |_path: axum::extract::Path<(String, String)>| {
                let bytes = bytes.clone();
                async move {
                    let body_bytes = (*bytes).clone();
                    let s = stream::once(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        Ok::<_, std::io::Error>(::bytes::Bytes::from(body_bytes))
                    });
                    Body::from_stream(s).into_response()
                }
            }),
        );
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
        let _restore = EnvVarGuard("NEVOFLUX_ENGINE_MIRROR_BASE");

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
        assert_eq!(m.kind, "windows-x64-cpu");
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

        // Task 2.4 review finding M2: re-running install() for the exact
        // same (tag, kind, root) must succeed as the ordinary
        // already-installed case, not clobber or duplicate the marker.
        let mut original = marker::read_marker(&final_dir).unwrap();
        original.installed_at = 999_999; // a sentinel value nothing else produces
        marker::write_marker(&final_dir, &original).unwrap();

        let second_run = install(&rel, &kind, root_dir.path(), &cancel, &mut |_| {}).await;
        assert_eq!(second_run.as_deref(), Ok(final_dir.as_path()));
        let after = marker::read_marker(&final_dir).unwrap();
        assert_eq!(
            after.installed_at, 999_999,
            "an already-installed re-run must leave the existing marker untouched, not rewrite it"
        );
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
        let _restore = EnvVarGuard("NEVOFLUX_ENGINE_MIRROR_BASE");

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

        // Coordinator clarification: v3 §7.4 step 2 requires a failed sha
        // check to DELETE the bad archive, not merely reject and leave it
        // on disk to be resumed forever against the same wrong pin.
        // `models::fetch::fetch_to` already deletes its `.part` on a
        // digest mismatch (`fetch.rs:254-259`) before ever renaming to the
        // final staged path, so neither should exist here.
        let staged = root_dir.path().join(".staging").join("test-cpu-bad.zip");
        assert!(!staged.exists(), "the bad archive must not survive");
        let staged_part = root_dir
            .path()
            .join(".staging")
            .join("test-cpu-bad.zip.part");
        assert!(
            !staged_part.exists(),
            "the failed .part download must not survive either"
        );
    }

    #[tokio::test]
    async fn install_stops_promptly_when_the_token_is_already_cancelled() {
        let _guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let zip_bytes = build_zip(&[("pkg/llama-server.exe", b"fake-engine-binary")]);
        let sha = sha256_hex(&zip_bytes);
        let base_url = spawn_delayed_fake_mirror(zip_bytes.clone()).await;
        std::env::set_var("NEVOFLUX_ENGINE_MIRROR_BASE", &base_url);
        let _restore = EnvVarGuard("NEVOFLUX_ENGINE_MIRROR_BASE");

        let archive = release::EngineArchive {
            platform: "windows-x64",
            backend: Backend::Cpu,
            variant: None,
            asset: EngineAsset {
                name: "test-cpu-cancel.zip",
                bytes: zip_bytes.len() as u64,
                sha256: leak_str(sha),
            },
        };
        let rel = EngineRelease {
            tag: "test-tag-cancel",
            upstream_tag: "test-tag-cancel",
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
        cancel.cancel(); // already cancelled before install() even starts
        let kind = InstallKind {
            platform: "windows-x64".to_string(),
            backend: Backend::Cpu,
            variant: None,
            cudart: None,
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            install(&rel, &kind, root_dir.path(), &cancel, &mut |_| {}),
        )
        .await
        .expect("install must return promptly rather than hang when cancelled");

        assert!(
            result.is_err(),
            "a cancelled install must not succeed: {result:?}"
        );
        let final_dir = install_dir(root_dir.path(), rel.tag, &kind);
        assert!(!final_dir.exists());
    }

    /// Task 2.4 re-review finding M2(b): every terminal failure must clean
    /// up BOTH the extraction directory and every staged archive, not just
    /// one. This exercises a failure that happens AFTER a successful,
    /// sha-verified download (unlike the sha-mismatch test above, where no
    /// staged archive or tmp dir ever exists at all) -- a zip that
    /// downloads and extracts fine but contains no `llama-server.exe`, so
    /// `sentinels_ok` fails with a fully staged archive AND a populated
    /// `tmp_dir` both present at the moment of failure. `.staging/` must
    /// end up completely empty afterward: no leftover archive, no `.tmp`.
    #[tokio::test]
    async fn install_leaves_the_staging_dir_empty_after_a_sentinel_failure() {
        let _guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let zip_bytes = build_zip(&[("pkg/not-the-engine-binary.txt", b"nope")]);
        let sha = sha256_hex(&zip_bytes);
        let mut files = HashMap::new();
        files.insert("test-cpu-sentinel-fail.zip".to_string(), zip_bytes.clone());
        let base_url = spawn_fake_mirror(files).await;
        std::env::set_var("NEVOFLUX_ENGINE_MIRROR_BASE", &base_url);
        let _restore = EnvVarGuard("NEVOFLUX_ENGINE_MIRROR_BASE");

        let archive = release::EngineArchive {
            platform: "windows-x64",
            backend: Backend::Cpu,
            variant: None,
            asset: EngineAsset {
                name: "test-cpu-sentinel-fail.zip",
                bytes: zip_bytes.len() as u64,
                sha256: leak_str(sha),
            },
        };
        let rel = EngineRelease {
            tag: "test-tag-sentinel-fail",
            upstream_tag: "test-tag-sentinel-fail",
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
            matches!(result, Err(LocalError::SentinelMissing { .. })),
            "{result:?}"
        );

        let staging = root_dir.path().join(".staging");
        let remaining: Vec<_> = std::fs::read_dir(&staging)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert!(
            remaining.is_empty(),
            "staging dir must be empty after a failed install, found: {remaining:?}"
        );
    }
}
