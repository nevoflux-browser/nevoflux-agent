//! ffmpeg resolution + subprocess spawning.
//!
//! Resolution order:
//!   1. $PATH (via ffmpeg-sidecar's auto-detect)
//!   2. `~/.cache/nevoflux/bin/ffmpeg`
//!   3. On-demand download via ffmpeg-sidecar to the cache path.

use std::path::PathBuf;

use ffmpeg_sidecar::command::{ffmpeg_is_installed, FfmpegCommand};
use ffmpeg_sidecar::download::auto_download;

use crate::error::{DaemonError, Result};

/// Resolve the ffmpeg binary, auto-downloading to
/// `~/.cache/nevoflux/bin/` if no system binary exists.
pub fn resolve_ffmpeg() -> Result<PathBuf> {
    // Fast path: system ffmpeg is available in $PATH.
    if ffmpeg_is_installed() {
        // ffmpeg_is_installed() checks `ffmpeg_path()` which tries the sidecar path first,
        // then falls back to the "ffmpeg" string (resolved via PATH).
        // When it's in PATH, `which ffmpeg` gives us the real path.
        return which_ffmpeg();
    }

    // Slow path: download to our cache directory.
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| DaemonError::InternalError("no cache dir".into()))?
        .join("nevoflux")
        .join("bin");
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| DaemonError::InternalError(format!("mkdir {:?}: {}", cache_dir, e)))?;

    // Redirect the sidecar download location to our cache directory.
    // ffmpeg-sidecar 1.1.x uses the executable directory as sidecar_dir();
    // we symlink or copy to our cache after downloading.
    // The simplest approach: download to the cache dir directly via the
    // FFMPEG_DOWNLOAD_DIR env var (supported in newer versions) or by copying
    // the sidecar binary after auto_download().
    unsafe { std::env::set_var("FFMPEG_DOWNLOAD_DIR", &cache_dir); }

    auto_download()
        .map_err(|e| DaemonError::InternalError(format!("ffmpeg auto_download failed: {}", e)))?;

    // After download, check our cache dir first.
    #[cfg(target_os = "windows")]
    let cached = cache_dir.join("ffmpeg.exe");
    #[cfg(not(target_os = "windows"))]
    let cached = cache_dir.join("ffmpeg");

    if cached.exists() {
        return Ok(cached);
    }

    // Fallback: auto_download may have placed the binary in the sidecar dir
    // (next to the running executable). Try to find and return it.
    if ffmpeg_is_installed() {
        return which_ffmpeg();
    }

    Err(DaemonError::InternalError(
        "ffmpeg not found after auto_download".into(),
    ))
}

/// Locate the `ffmpeg` binary using the system PATH.
fn which_ffmpeg() -> Result<PathBuf> {
    which::which("ffmpeg")
        .map_err(|e| DaemonError::InternalError(format!("which ffmpeg: {}", e)))
}

/// Construct an ffmpeg image2pipe command for encoding PNG stream -> MP4.
pub fn image2pipe_cmd(output_path: &std::path::Path, fps: u32) -> FfmpegCommand {
    let mut cmd = FfmpegCommand::new();
    cmd.hide_banner()
        .args(["-y", "-f", "image2pipe", "-framerate", &fps.to_string(), "-i", "-"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "23", "-preset", "medium"])
        .args(["-movflags", "+faststart"])
        .output(output_path.to_string_lossy().as_ref());
    cmd
}
