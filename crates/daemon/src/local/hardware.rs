//! Hardware probing and install-kind (backend/variant) selection for
//! on-device inference (v3 §4.2/§4.3/§9/§10).
//!
//! [`probe`] fingerprints the host once (NVIDIA GPUs via `nvidia-smi`,
//! Vulkan/cudart runtime presence, system RAM) and caches the result —
//! nothing in this module runs at daemon startup; a later task calls
//! [`probe`] on demand (e.g. from the engine supervisor, before it decides
//! what to install/launch) and reads [`cached`] afterwards. [`fallback_chain`]
//! is pure: given a [`HardwareProbe`] and a [`crate::local::config::BackendPref`],
//! it returns the ordered list of [`InstallKind`]s to try, most-preferred
//! first.

use std::sync::Mutex;
use std::time::Duration;

use crate::local::config::BackendPref;

/// One NVIDIA GPU as reported by `nvidia-smi`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct GpuInfo {
    pub name: String,
    pub vram_bytes: u64,
    pub compute_cap: Option<(u32, u32)>,
}

/// A fingerprint of the host's inference-relevant hardware, produced by
/// [`probe`].
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct HardwareProbe {
    pub os: String,
    pub arch: String,
    pub nvidia_gpus: Vec<GpuInfo>,
    pub has_physical_nvidia: bool,
    pub has_usable_nvidia: bool,
    pub driver_cuda_version: Option<(u32, u32)>,
    pub cuda_runtime_lines: Vec<u32>,
    pub vulkan_available: bool,
    pub ram_bytes: u64,
    pub macos_version: Option<String>,
}

/// Compute backend a launched engine process actually uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    Cpu,
    Vulkan,
    Cuda,
    Metal,
}

/// One buildable/downloadable engine variant: a platform, a backend, and
/// (for CUDA) the specific prebuilt archive variant + cudart runtime it
/// needs.
///
/// Uses owned `String`s rather than `&'static str` (unlike the release
/// constants in `crate::local::release`, added by Task 2.3) because a value
/// of this type round-trips through `marker.json` (Task 2.4): it is
/// `serde_json::to_string`'d when an install completes and
/// `serde_json::from_str`'d back on the next startup, and `&'static str`
/// cannot deserialize from owned JSON text (controller ruling R5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct InstallKind {
    pub platform: String,
    pub backend: Backend,
    pub variant: Option<String>,
    pub cudart: Option<String>,
}

/// Per-subprocess-call cap (`nvidia-smi`, `sysctl`, `sw_vers`) — a hung or
/// missing binary must not stall an individual call past this. Because
/// [`probe_nvidia`] alone can invoke `nvidia-smi` up to three times, this
/// is a secondary safety net; [`PROBE_TOTAL_BUDGET`] is what actually
/// bounds one [`probe`] call end to end.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Total wall-clock budget for one entire [`probe`] call, covering every
/// subprocess it may spawn internally. A per-call-only cap does not bound
/// the whole probe: `nvidia-smi` is invoked up to three times (the
/// `compute_cap` query, its cap-less retry, and the plain-header query)
/// and macOS adds two more (`sysctl`, `sw_vers`), so a per-invocation-only
/// [`PROBE_TIMEOUT`] could total 25s in the worst case. [`probe`] instead
/// wraps the whole gathering sequence in one `tokio::time::timeout` at
/// this budget and returns a bare os/arch fingerprint (everything else
/// defaulted) if it fires.
const PROBE_TOTAL_BUDGET: Duration = Duration::from_secs(5);

static CACHED: Mutex<Option<HardwareProbe>> = Mutex::new(None);

/// Fingerprint this host's hardware. Never called automatically at daemon
/// startup (v3 §10) — only on demand, by whichever later task needs to
/// decide what to install/launch. Updates [`cached`]'s value. Bounded end
/// to end by [`PROBE_TOTAL_BUDGET`], regardless of how many subprocesses
/// or filesystem probes it runs internally.
pub async fn probe() -> HardwareProbe {
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();

    let result = match tokio::time::timeout(
        PROBE_TOTAL_BUDGET,
        probe_inner(os.clone(), arch.clone()),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                budget_secs = PROBE_TOTAL_BUDGET.as_secs(),
                %os,
                %arch,
                "local::hardware::probe exceeded its total time budget; returning a bare os/arch fingerprint with everything else defaulted"
            );
            HardwareProbe {
                os,
                arch,
                ..Default::default()
            }
        }
    };

    tracing::debug!(?result, "local::hardware::probe complete");

    if let Ok(mut guard) = CACHED.lock() {
        *guard = Some(result.clone());
    }

    result
}

/// The actual gathering sequence, factored out of [`probe`] so the latter
/// can wrap it in one [`PROBE_TOTAL_BUDGET`]-wide timeout.
async fn probe_inner(os: String, arch: String) -> HardwareProbe {
    let (nvidia_gpus, driver_cuda_version) = probe_nvidia().await;
    let has_physical_nvidia = !nvidia_gpus.is_empty();
    let cuda_visible_devices = std::env::var("CUDA_VISIBLE_DEVICES").ok();
    let has_usable_nvidia =
        has_physical_nvidia && !visible_devices_hide_all(cuda_visible_devices.as_deref());

    // Both do synchronous filesystem globbing (up to eight `glob::glob`
    // walks on Linux: 2 cudart majors + Vulkan loader + Vulkan ICD, each
    // over `/usr/lib*`/`/lib*`/icd.d) — never call them directly on the
    // async executor.
    let cuda_runtime_lines = {
        let os = os.clone();
        tokio::task::spawn_blocking(move || probe_cuda_runtime_lines(&os))
            .await
            .unwrap_or_default()
    };
    let vulkan_available = {
        let os = os.clone();
        tokio::task::spawn_blocking(move || probe_vulkan_available(&os))
            .await
            .unwrap_or(false)
    };
    let ram_bytes = probe_ram_bytes(&os).await;
    let macos_version = probe_macos_version(&os).await;

    HardwareProbe {
        os,
        arch,
        nvidia_gpus,
        has_physical_nvidia,
        has_usable_nvidia,
        driver_cuda_version,
        cuda_runtime_lines,
        vulkan_available,
        ram_bytes,
        macos_version,
    }
}

/// The result of the most recent [`probe`] call, if any has run yet in this
/// process.
pub fn cached() -> Option<HardwareProbe> {
    CACHED.lock().ok().and_then(|guard| guard.clone())
}

/// Runs `program args…`, capped at [`PROBE_TIMEOUT`]. `None` if the binary
/// is missing, the process errors, times out, or exits non-zero — a probe
/// failure is always "signal unavailable", never a panic. `kill_on_drop`
/// is set so a call cancelled by [`PROBE_TOTAL_BUDGET`]'s outer timeout
/// (which drops this future mid-flight) does not leave the child process
/// running in the background.
async fn run_with_timeout(program: &str, args: &[&str]) -> Option<String> {
    let output_fut = tokio::process::Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(PROBE_TIMEOUT, output_fut).await {
        Ok(Ok(output)) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(Ok(output)) => {
            tracing::debug!(
                program,
                ?args,
                exit_code = output.status.code(),
                "probe subprocess exited non-zero"
            );
            None
        }
        Ok(Err(e)) => {
            tracing::debug!(program, ?args, error = %e, "probe subprocess failed to spawn");
            None
        }
        Err(_) => {
            tracing::debug!(
                program,
                ?args,
                timeout_secs = PROBE_TIMEOUT.as_secs(),
                "probe subprocess timed out"
            );
            None
        }
    }
}

/// Runs the `nvidia-smi` invocations the probe needs: the queried CSV (GPU
/// list, with a cap-less retry — see below) and the plain header (driver
/// CUDA version).
///
/// An `nvidia-smi` old enough (~pre-11.x) to reject the `compute_cap`
/// field exits non-zero on the first query, which would otherwise blank
/// the whole GPU list and make `has_physical_nvidia` false even though a
/// real card is present — silently defeating v3 §10's "physical but
/// unusable" distinction before it can ever fire. If the first query comes
/// back empty, retry without `compute_cap`; a GPU found this way just
/// carries `compute_cap: None` (routes to [`CudaBucket::Portable`] in
/// [`select_cuda_kind`]).
async fn probe_nvidia() -> (Vec<GpuInfo>, Option<(u32, u32)>) {
    let mut gpus = run_with_timeout(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,compute_cap",
            "--format=csv,noheader,nounits",
        ],
    )
    .await
    .as_deref()
    .map(parse_nvidia_smi_csv)
    .unwrap_or_default();

    if gpus.is_empty() {
        tracing::debug!(
            "nvidia-smi --query-gpu=...,compute_cap returned nothing; retrying without compute_cap"
        );
        gpus = run_with_timeout(
            "nvidia-smi",
            &[
                "--query-gpu=name,memory.total",
                "--format=csv,noheader,nounits",
            ],
        )
        .await
        .as_deref()
        .map(parse_nvidia_smi_csv)
        .unwrap_or_default();
    }
    if gpus.is_empty() {
        tracing::debug!("nvidia-smi unavailable, or reports no GPUs, on both queries");
    }

    let plain = run_with_timeout("nvidia-smi", &[]).await;
    let driver_cuda_version = plain.as_deref().and_then(parse_driver_cuda_version);
    if !gpus.is_empty() && driver_cuda_version.is_none() {
        tracing::debug!(
            "nvidia-smi listed GPU(s) but no driver CUDA version could be parsed from its plain header"
        );
    }

    (gpus, driver_cuda_version)
}

/// Linux only: which of `libcudart.so.12`/`.13` are present on the system
/// (Windows CUDA archives bundle their own cudart, so this is never
/// consulted there — see [`select_cuda_kind`]).
fn probe_cuda_runtime_lines(os: &str) -> Vec<u32> {
    if os != "linux" {
        return Vec::new();
    }
    [12u32, 13u32]
        .into_iter()
        .filter(|major| find_library(&format!("libcudart.so.{major}")))
        .collect()
}

/// v3 §10 specifies "loader + ICD" — a loader with no actual driver behind
/// it cannot run anything. Linux checks both (loader file + at least one
/// ICD manifest). Windows checks the loader only: a full check would read
/// the Vulkan ICD registry key (`HKLM\SOFTWARE\Khronos\Vulkan\Drivers`),
/// which needs a `windows-sys` feature beyond what this task's brief
/// authorized (`Win32_System_SystemInformation` only) — left as a known
/// gap (recorded for the controller). Either way a stale loader with no
/// usable ICD still degrades safely at launch time via v3 §9's
/// `BackendUnavailable` → next-tier path, so this is a diagnostic-accuracy
/// gap, not a correctness one.
fn probe_vulkan_available(os: &str) -> bool {
    match os {
        "windows" => windows_vulkan_available(),
        "linux" => find_library("libvulkan.so.1") && linux_vulkan_icd_present(),
        // macOS's selection never considers Vulkan (see `fallback_chain`) —
        // the build matrix has no macOS Vulkan archive.
        _ => false,
    }
}

fn windows_vulkan_available() -> bool {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    std::path::Path::new(&system_root)
        .join("System32")
        .join("vulkan-1.dll")
        .exists()
}

/// Whether at least one Vulkan ICD manifest is discoverable via the
/// standard Vulkan Loader search locations, or an explicit
/// `VK_ICD_FILENAMES`/`VK_ADD_DRIVER_FILES` override is set. The loader
/// (`libvulkan.so.1`) can be installed with no GPU driver behind it at
/// all — checking for it alone would report `true` on a machine that
/// cannot actually create a Vulkan device.
fn linux_vulkan_icd_present() -> bool {
    if std::env::var("VK_ICD_FILENAMES").is_ok() || std::env::var("VK_ADD_DRIVER_FILES").is_ok() {
        return true;
    }
    for dir in [
        "/usr/share/vulkan/icd.d",
        "/etc/vulkan/icd.d",
        "/usr/local/share/vulkan/icd.d",
    ] {
        let pattern = format!("{dir}/*.json");
        if let Ok(mut paths) = glob::glob(&pattern) {
            if paths.any(|p| p.is_ok()) {
                return true;
            }
        }
    }
    false
}

/// Looks for `filename` under `/usr/lib*`, `/lib*`, or any directory named
/// in `LD_LIBRARY_PATH`. Linux-only search paths; harmless (always `false`)
/// on other platforms since none of them are real directories there.
fn find_library(filename: &str) -> bool {
    for pattern in [format!("/usr/lib*/{filename}"), format!("/lib*/{filename}")] {
        if let Ok(mut paths) = glob::glob(&pattern) {
            if paths.any(|p| p.is_ok()) {
                return true;
            }
        }
    }
    if let Ok(ld_path) = std::env::var("LD_LIBRARY_PATH") {
        for dir in ld_path.split(':') {
            if !dir.is_empty() && std::path::Path::new(dir).join(filename).exists() {
                return true;
            }
        }
    }
    false
}

async fn probe_ram_bytes(os: &str) -> u64 {
    match os {
        "windows" => windows_ram_bytes(),
        "linux" => linux_ram_bytes().await,
        "macos" => macos_ram_bytes().await,
        _ => 0,
    }
}

async fn linux_ram_bytes() -> u64 {
    tokio::fs::read_to_string("/proc/meminfo")
        .await
        .ok()
        .and_then(|content| parse_proc_meminfo(&content))
        .unwrap_or(0)
}

async fn macos_ram_bytes() -> u64 {
    run_with_timeout("sysctl", &["-n", "hw.memsize"])
        .await
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(windows)]
fn windows_ram_bytes() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: `status` is a valid `MEMORYSTATUSEX` with `dwLength` set to
    // its own size, as `GlobalMemoryStatusEx` requires; the pointer is a
    // live, uniquely-borrowed local.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok != 0 {
        status.ullTotalPhys
    } else {
        0
    }
}

#[cfg(not(windows))]
fn windows_ram_bytes() -> u64 {
    0
}

async fn probe_macos_version(os: &str) -> Option<String> {
    if os != "macos" {
        return None;
    }
    run_with_timeout("sw_vers", &["-productVersion"])
        .await
        .map(|s| s.trim().to_string())
}

/// Parses `nvidia-smi --query-gpu=name,memory.total,compute_cap
/// --format=csv,noheader,nounits` output (one GPU per line, `memory.total`
/// in MiB). Also accepts the 2-field `name,memory.total` shape (no
/// `compute_cap` column, `compute_cap: None`) — [`probe_nvidia`] retries
/// with that narrower query when a GPU is present but the fuller one comes
/// back empty (an nvidia-smi old enough to reject `compute_cap` outright
/// must not erase the fact that a physical NVIDIA card exists). Malformed
/// lines are skipped rather than causing a panic or an `Err` — a
/// partially-parsed GPU list is more useful than none. `memory.total *
/// MIB` is `checked_mul`'d: a corrupted or hostile `nvidia-smi` reporting
/// an absurd MiB count must not overflow `u64` (panic in debug, wraparound
/// in release) — the line is skipped instead.
pub fn parse_nvidia_smi_csv(csv: &str) -> Vec<GpuInfo> {
    csv.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            let (name, mem, cap) = match fields.as_slice() {
                [name, mem, cap] => (*name, *mem, Some(*cap)),
                [name, mem] => (*name, *mem, None),
                _ => return None,
            };
            let vram_mib: u64 = mem.parse().ok()?;
            let vram_bytes = vram_mib.checked_mul(crate::local::memory::MIB)?;
            Some(GpuInfo {
                name: name.to_string(),
                vram_bytes,
                compute_cap: cap.and_then(parse_compute_cap),
            })
        })
        .collect()
}

fn parse_compute_cap(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.splitn(2, '.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    Some((major, minor))
}

/// Extracts the driver's reported CUDA runtime version from plain
/// `nvidia-smi` output (no `--query-gpu`), e.g. a header line containing
/// `CUDA Version: 13.0` → `Some((13, 0))`. `None` if the marker is absent
/// or its value isn't a `major.minor` number (e.g. a driver too old to
/// report one prints `CUDA Version: N/A`).
///
/// Native drivers print `CUDA Version: 13.0`; a paravirtualized/WSL-style
/// KMD/UMD split driver (confirmed live on this machine's Tesla T4 VM)
/// prints `CUDA UMD Version: 13.3` instead — both are matched by looking,
/// on each line, for the first `Version:` that follows the word `CUDA`.
pub fn parse_driver_cuda_version(nvidia_smi_plain: &str) -> Option<(u32, u32)> {
    for line in nvidia_smi_plain.lines() {
        let cuda_idx = match line.find("CUDA") {
            Some(i) => i,
            None => continue,
        };
        let after_cuda = &line[cuda_idx..];
        let Some(version_idx) = after_cuda.find("Version:") else {
            continue;
        };
        let after_version = &after_cuda[version_idx + "Version:".len()..];
        let Some(token) = after_version
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '|')
            .next()
        else {
            continue;
        };
        if let Some(v) = parse_compute_cap(token) {
            return Some(v);
        }
    }
    None
}

/// Whether a `CUDA_VISIBLE_DEVICES` value hides every device: unset is
/// "not hidden" (`false`); `""`, `"-1"`, or (case-insensitively) `"none"`
/// hide all.
pub fn visible_devices_hide_all(env_value: Option<&str>) -> bool {
    match env_value {
        None => false,
        Some(v) => {
            let v = v.trim();
            v.is_empty() || v == "-1" || v.eq_ignore_ascii_case("none")
        }
    }
}

/// Parses `MemTotal:` out of `/proc/meminfo` content, returning bytes (the
/// file reports KiB despite the `kB` label, per `man proc`). `* 1024` is
/// `checked_mul`'d — `/proc/meminfo` is a kernel-controlled file in
/// practice, but the parse still treats its number as untrusted external
/// input rather than assume it can never overflow `u64`.
pub fn parse_proc_meminfo(content: &str) -> Option<u64> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kib: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
            return kib.checked_mul(1024);
        }
    }
    None
}

/// Compute-capability bucket driving the CUDA archive variant suffix (v3
/// §4.2/§4.3). No `cuda13-legacy` archive exists in the build matrix, so
/// `Legacy` is special-cased in [`select_cuda_kind`] to always mean CUDA 12
/// regardless of the driver's own major version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CudaBucket {
    Legacy,
    Older,
    Newer,
    Portable,
}

fn cuda_bucket(compute_cap: Option<(u32, u32)>) -> CudaBucket {
    match compute_cap {
        Some((6, 1)) => CudaBucket::Legacy,
        Some((7, 5)) | Some((8, 0)) | Some((8, 6)) | Some((8, 9)) => CudaBucket::Older,
        Some((9, 0)) | Some((10, 0)) | Some((12, 0)) => CudaBucket::Newer,
        _ => CudaBucket::Portable,
    }
}

/// The cudart toolkit version the named variant's prebuilt Windows archive
/// was compiled against, per the pinned engine release's `BUILD_INFO.txt`.
/// Only `cuda13-older` — this machine's own selection — has been measured
/// against a real archive so far (task-2.2-brief.md's verified facts); the
/// rest are filled in once Task 2.3 generates `release.rs` from every
/// archive's own `BUILD_INFO.txt` and can supersede this table.
///
/// **Contract:** `None` here means "not yet measured", never "no cudart
/// needed" — per v3 §4.3, every real Windows CUDA archive requires a
/// paired cudart archive. Tasks 2.3/2.4 MUST treat a Windows
/// `Backend::Cuda` [`InstallKind`] whose `cudart` is `None` as *unusable*
/// (skip to the next tier), not as "nothing to pair".
fn known_cudart_version(variant: &str) -> Option<&'static str> {
    match variant {
        "cuda13-older" => Some("13.3"),
        _ => None,
    }
}

/// Whether the pinned engine release ships a CUDA archive for
/// `(platform, variant)` — see `task-2.3-asset-names.md`'s 28 staged
/// asset names (tag `b10909-mix-bea84f7`), which are the ground truth this
/// table mirrors:
/// - `linux-arm64` ships exactly one CUDA archive, `cuda13-portable` — no
///   CUDA-12 line at all.
/// - `linux-x64` and `windows-x64` each ship the full
///   `cuda12-{legacy,older,newer,portable}` +
///   `cuda13-{older,newer,portable}` set (seven variants each;
///   `cuda12-portable` is real — controller ruling R45 — do not remove it).
/// - `windows-arm64` and macOS ship no CUDA archive at all (`fallback_chain`
///   never reaches [`select_cuda_kind`] for either).
///
/// A platform-blind selector could otherwise name a `(platform, variant)`
/// pair with no matching download — a hard v3 §9 `DownloadFailed` stop at
/// install time, not a safe `BackendUnavailable` degrade. This guard turns
/// that into a `None` from [`select_cuda_kind`], which falls through to
/// Vulkan/CPU instead (controller ruling R45).
fn cuda_variant_exists_on(platform: &str, variant: &str) -> bool {
    match platform {
        "linux-arm64" => variant == "cuda13-portable",
        "linux-x64" | "windows-x64" => matches!(
            variant,
            "cuda12-legacy"
                | "cuda12-older"
                | "cuda12-newer"
                | "cuda12-portable"
                | "cuda13-older"
                | "cuda13-newer"
                | "cuda13-portable"
        ),
        _ => false,
    }
}

fn arch_short(arch: &str) -> &str {
    match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn platform_string(p: &HardwareProbe) -> String {
    format!("{}-{}", p.os, arch_short(&p.arch))
}

fn cpu_kind(p: &HardwareProbe) -> InstallKind {
    InstallKind {
        platform: platform_string(p),
        backend: Backend::Cpu,
        variant: None,
        cudart: None,
    }
}

fn vulkan_kind(p: &HardwareProbe) -> InstallKind {
    InstallKind {
        platform: platform_string(p),
        backend: Backend::Vulkan,
        variant: None,
        cudart: None,
    }
}

fn metal_kind(p: &HardwareProbe) -> InstallKind {
    InstallKind {
        platform: platform_string(p),
        backend: Backend::Metal,
        variant: None,
        cudart: None,
    }
}

/// Picks the best CUDA [`InstallKind`] for `p`, or `None` if CUDA cannot be
/// used at all: no usable NVIDIA GPU, a driver too old to report CUDA
/// major >= 12, or — on Linux only — no matching `libcudart.so.<major>` on
/// the system (Windows archives bundle their own cudart, so no system
/// check applies there).
fn select_cuda_kind(p: &HardwareProbe) -> Option<InstallKind> {
    if !p.has_usable_nvidia {
        return None;
    }
    let (driver_major, _) = p.driver_cuda_version?;
    if driver_major < 12 {
        return None;
    }
    let gpu = p.nvidia_gpus.first()?;
    let bucket = cuda_bucket(gpu.compute_cap);

    // No `cuda13-legacy` variant exists in the build matrix: compute
    // capability 6.1 (Pascal) tops out at CUDA 12 regardless of what the
    // installed driver otherwise reports.
    let major = if bucket == CudaBucket::Legacy {
        12
    } else if driver_major >= 13 {
        13
    } else {
        12
    };

    if p.os == "linux" && !p.cuda_runtime_lines.contains(&major) {
        return None;
    }

    let variant = match (major, bucket) {
        (12, CudaBucket::Legacy) => "cuda12-legacy",
        (12, CudaBucket::Older) => "cuda12-older",
        (12, CudaBucket::Newer) => "cuda12-newer",
        (12, CudaBucket::Portable) => "cuda12-portable",
        (13, CudaBucket::Older) => "cuda13-older",
        (13, CudaBucket::Newer) => "cuda13-newer",
        (13, CudaBucket::Portable) => "cuda13-portable",
        // (13, Legacy) is unreachable: `major` is forced to 12 above
        // whenever `bucket == Legacy`.
        _ => return None,
    };

    let platform = platform_string(p);
    if !cuda_variant_exists_on(&platform, variant) {
        // The compute-cap/driver-major math named a real variant string,
        // but this platform has no such archive (e.g. `cuda12-portable` on
        // `linux-arm64`, which ships only `cuda13-portable`). Fall through
        // to Vulkan/CPU rather than send `fallback_chain`'s caller toward a
        // download that does not exist.
        tracing::debug!(
            platform,
            variant,
            "computed CUDA variant has no archive on this platform; falling through"
        );
        return None;
    }

    Some(InstallKind {
        platform,
        backend: Backend::Cuda,
        variant: Some(variant.to_string()),
        cudart: known_cudart_version(variant).map(str::to_string),
    })
}

/// Ordered list of [`InstallKind`]s to try, most-preferred first, for `p`
/// under backend preference `pref`. See the module doc and
/// `task-2.2-brief.md`'s "Selection rules" for the full rule set (driver
/// CUDA major, compute-capability bucket, the Linux cudart-presence
/// requirement, and the macOS/Windows-arm64/hidden-NVIDIA platform
/// overrides).
///
/// **macOS contract note (controller review, fix round 1):** the returned
/// `InstallKind` always reports `backend: Metal` on macOS, even under an
/// explicit `BackendPref::Cpu` — correct at *install* granularity (there is
/// one macOS archive, "Metal 内含" its own CPU fallback in-process, so
/// there is nothing else to fetch), but it means the explicit CPU
/// preference is not visible in this chain's output. A consumer deriving
/// launch flags (e.g. `n_gpu_layers`) must read the *original* `pref`
/// passed in here, not `InstallKind.backend`, to honor an explicit
/// `BackendPref::Cpu` on macOS.
pub fn fallback_chain(p: &HardwareProbe, pref: BackendPref) -> Vec<InstallKind> {
    // Platform-locked backends: neither depends on hardware detection nor
    // is affected by `pref` — there is nothing else on these platforms to
    // fall back to (v3 §9).
    if p.os == "macos" {
        return vec![metal_kind(p)];
    }
    if p.os == "windows" && p.arch == "aarch64" {
        return vec![cpu_kind(p)];
    }

    // An NVIDIA GPU physically present but made unusable (today, only by
    // `CUDA_VISIBLE_DEVICES` hiding every device) forces CPU-only — but
    // ONLY for automatic selection (controller ruling R44). v3 §10's rule
    // feeds §9's automatic 回退链, not an explicit user preference: §20/
    // §21.4 expose `backend` as a user-facing `auto|cpu|vulkan|cuda`
    // select whose whole purpose is overriding automatic selection, and
    // `CUDA_VISIBLE_DEVICES` (the only unusability signal here) is a
    // CUDA-runtime variable the Vulkan loader/ICD path never reads. An
    // explicit `Vulkan`/`Cuda` pref still gets its normal validity check
    // below; only `Auto` is short-circuited here.
    if matches!(pref, BackendPref::Auto) && p.has_physical_nvidia && !p.has_usable_nvidia {
        return vec![cpu_kind(p)];
    }

    match pref {
        BackendPref::Cpu => vec![cpu_kind(p)],
        BackendPref::Vulkan => {
            if p.vulkan_available {
                vec![vulkan_kind(p), cpu_kind(p)]
            } else {
                vec![cpu_kind(p)]
            }
        }
        BackendPref::Cuda => match select_cuda_kind(p) {
            Some(k) => vec![k, cpu_kind(p)],
            None => vec![cpu_kind(p)],
        },
        BackendPref::Auto => {
            let mut chain = Vec::new();
            if let Some(k) = select_cuda_kind(p) {
                chain.push(k);
            }
            if p.vulkan_available {
                chain.push(vulkan_kind(p));
            }
            chain.push(cpu_kind(p));
            chain
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_nvidia_smi_csv -------------------------------------------

    #[test]
    fn parses_a_t4_line_from_nvidia_smi_csv() {
        let gpus = parse_nvidia_smi_csv("Tesla T4, 15360, 7.5");
        assert_eq!(
            gpus,
            vec![GpuInfo {
                name: "Tesla T4".to_string(),
                vram_bytes: 15360 * crate::local::memory::MIB,
                compute_cap: Some((7, 5)),
            }]
        );
    }

    #[test]
    fn parses_multiple_gpu_lines() {
        let gpus =
            parse_nvidia_smi_csv("Tesla T4, 15360, 7.5\nNVIDIA GeForce RTX 5090, 32768, 12.0\n");
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[1].name, "NVIDIA GeForce RTX 5090");
        assert_eq!(gpus[1].compute_cap, Some((12, 0)));
    }

    #[test]
    fn skips_malformed_csv_lines_without_panicking() {
        let gpus = parse_nvidia_smi_csv("not,even,close,to,valid\n\nTesla T4, 15360, 7.5");
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "Tesla T4");
    }

    #[test]
    fn parses_the_cap_less_fallback_csv_shape() {
        // `probe_nvidia`'s retry query (`name,memory.total`, no
        // `compute_cap`) for an nvidia-smi old enough to reject that field.
        let gpus = parse_nvidia_smi_csv("Tesla T4, 15360");
        assert_eq!(
            gpus,
            vec![GpuInfo {
                name: "Tesla T4".to_string(),
                vram_bytes: 15360 * crate::local::memory::MIB,
                compute_cap: None,
            }]
        );
    }

    #[test]
    fn nvidia_smi_csv_rejects_a_vram_value_that_would_overflow_bytes() {
        // 20,000,000,000,000 MiB * 1 MiB overflows u64 (checked_mul must
        // catch it — the old unchecked `*` would panic in debug builds and
        // silently wrap in release).
        let gpus = parse_nvidia_smi_csv("Corrupted, 20000000000000, 7.5");
        assert!(gpus.is_empty());
    }

    #[test]
    fn parse_compute_cap_rejects_bracketed_not_available() {
        // A real nvidia-smi quirk for an unsupported/disabled GPU.
        assert_eq!(parse_compute_cap("[N/A]"), None);
    }

    // --- parse_driver_cuda_version ---------------------------------------

    #[test]
    fn extracts_cuda_version_from_plain_nvidia_smi_header() {
        let plain = "+-----------------------------------------------------------------------------------------+\n| NVIDIA-SMI 580.65.06              Driver Version: 580.65.06      CUDA Version: 13.0     |\n|-----------------------------------------+------------------------+----------------------+\n";
        assert_eq!(parse_driver_cuda_version(plain), Some((13, 0)));
    }

    #[test]
    fn extracts_cuda_version_from_a_paravirtualized_kmd_umd_header() {
        // Captured live from this machine's Tesla T4 VM (`nvidia-smi` with
        // no args): a WSL/GPU-passthrough-style driver reports "KMD
        // Version" + "CUDA UMD Version" instead of "Driver Version" +
        // "CUDA Version". Discovered by the live #[ignore] probe test,
        // which initially got `None` here against this exact line.
        let plain = "+-----------------------------------------------------------------------------------------+\n| NVIDIA-SMI 610.88                 KMD Version: 610.88        CUDA UMD Version: 13.3     |\n+-----------------------------------------+------------------------+----------------------+\n";
        assert_eq!(parse_driver_cuda_version(plain), Some((13, 3)));
    }

    #[test]
    fn returns_none_when_cuda_version_is_na() {
        let plain =
            "| NVIDIA-SMI 470.256.02   Driver Version: 470.256.02   CUDA Version: N/A       |";
        assert_eq!(parse_driver_cuda_version(plain), None);
    }

    #[test]
    fn returns_none_when_marker_absent() {
        assert_eq!(parse_driver_cuda_version("no nvidia-smi here"), None);
    }

    // --- visible_devices_hide_all -----------------------------------------

    #[test]
    fn visible_devices_unset_does_not_hide() {
        assert!(!visible_devices_hide_all(None));
    }

    #[test]
    fn visible_devices_empty_or_negative_one_or_none_hide_all() {
        assert!(visible_devices_hide_all(Some("")));
        assert!(visible_devices_hide_all(Some("-1")));
        assert!(visible_devices_hide_all(Some("none")));
        assert!(visible_devices_hide_all(Some("None")));
        assert!(visible_devices_hide_all(Some("NONE")));
    }

    #[test]
    fn visible_devices_a_device_list_does_not_hide() {
        assert!(!visible_devices_hide_all(Some("0")));
        assert!(!visible_devices_hide_all(Some("0,1")));
    }

    // --- parse_proc_meminfo -------------------------------------------------

    #[test]
    fn parses_memtotal_from_proc_meminfo() {
        let content = "MemTotal:       16374920 kB\nMemFree:         1234567 kB\nMemAvailable:    9876543 kB\n";
        assert_eq!(parse_proc_meminfo(content), Some(16_374_920 * 1024));
    }

    #[test]
    fn parse_proc_meminfo_returns_none_without_memtotal() {
        assert_eq!(parse_proc_meminfo("MemFree: 1234 kB\n"), None);
    }

    #[test]
    fn parse_proc_meminfo_rejects_a_value_that_would_overflow_bytes() {
        // 20,000,000,000,000,000 KiB * 1024 overflows u64.
        let content = "MemTotal:       20000000000000000 kB\n";
        assert_eq!(parse_proc_meminfo(content), None);
    }

    // --- fallback_chain: brief step-1 scenarios (a)-(f) --------------------

    fn windows_t4_probe() -> HardwareProbe {
        HardwareProbe {
            os: "windows".to_string(),
            arch: "x86_64".to_string(),
            nvidia_gpus: vec![GpuInfo {
                name: "Tesla T4".to_string(),
                vram_bytes: 15360 * crate::local::memory::MIB,
                compute_cap: Some((7, 5)),
            }],
            has_physical_nvidia: true,
            has_usable_nvidia: true,
            driver_cuda_version: Some((13, 0)),
            cuda_runtime_lines: vec![],
            vulkan_available: true,
            ram_bytes: 32 * crate::local::memory::GIB,
            macos_version: None,
        }
    }

    #[test]
    fn a_windows_t4_driver_13_selects_cuda13_older_then_vulkan_then_cpu() {
        let p = windows_t4_probe();
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(
            chain,
            vec![
                InstallKind {
                    platform: "windows-x64".to_string(),
                    backend: Backend::Cuda,
                    variant: Some("cuda13-older".to_string()),
                    cudart: Some("13.3".to_string()),
                },
                InstallKind {
                    platform: "windows-x64".to_string(),
                    backend: Backend::Vulkan,
                    variant: None,
                    cudart: None,
                },
                InstallKind {
                    platform: "windows-x64".to_string(),
                    backend: Backend::Cpu,
                    variant: None,
                    cudart: None,
                },
            ]
        );
    }

    #[test]
    fn a_without_vulkan_available_omits_it_from_the_chain() {
        let mut p = windows_t4_probe();
        p.vulkan_available = false;
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(
            chain.iter().map(|k| k.backend).collect::<Vec<_>>(),
            vec![Backend::Cuda, Backend::Cpu]
        );
    }

    #[test]
    fn b_windows_rtx5090_cap_12_0_selects_newer_variant() {
        let mut p = windows_t4_probe();
        p.nvidia_gpus = vec![GpuInfo {
            name: "NVIDIA GeForce RTX 5090".to_string(),
            vram_bytes: 32 * crate::local::memory::GIB,
            compute_cap: Some((12, 0)),
        }];
        p.driver_cuda_version = Some((13, 0));
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(chain[0].variant.as_deref(), Some("cuda13-newer"));
    }

    #[test]
    fn c_gtx1080_cap_6_1_driver_12_selects_cuda12_legacy() {
        let mut p = windows_t4_probe();
        p.nvidia_gpus = vec![GpuInfo {
            name: "NVIDIA GeForce GTX 1080".to_string(),
            vram_bytes: 8 * crate::local::memory::GIB,
            compute_cap: Some((6, 1)),
        }];
        p.driver_cuda_version = Some((12, 4));
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(chain[0].variant.as_deref(), Some("cuda12-legacy"));
    }

    #[test]
    fn c_legacy_cap_is_forced_to_cuda12_even_under_a_cuda13_driver() {
        // No `cuda13-legacy` variant exists in the build matrix: a newer
        // driver does not change which archive a Pascal-class GPU needs.
        let mut p = windows_t4_probe();
        p.nvidia_gpus = vec![GpuInfo {
            name: "NVIDIA GeForce GTX 1080".to_string(),
            vram_bytes: 8 * crate::local::memory::GIB,
            compute_cap: Some((6, 1)),
        }];
        p.driver_cuda_version = Some((13, 0));
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(chain[0].variant.as_deref(), Some("cuda12-legacy"));
    }

    #[test]
    fn d_linux_nvidia_without_cudart_falls_back_to_vulkan_then_cpu() {
        let p = HardwareProbe {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            nvidia_gpus: vec![GpuInfo {
                name: "Tesla T4".to_string(),
                vram_bytes: 15360 * crate::local::memory::MIB,
                compute_cap: Some((7, 5)),
            }],
            has_physical_nvidia: true,
            has_usable_nvidia: true,
            driver_cuda_version: Some((13, 0)),
            cuda_runtime_lines: vec![], // no libcudart.so.{12,13} found
            vulkan_available: true,
            ram_bytes: 32 * crate::local::memory::GIB,
            macos_version: None,
        };
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(
            chain,
            vec![
                InstallKind {
                    platform: "linux-x64".to_string(),
                    backend: Backend::Vulkan,
                    variant: None,
                    cudart: None,
                },
                InstallKind {
                    platform: "linux-x64".to_string(),
                    backend: Backend::Cpu,
                    variant: None,
                    cudart: None,
                },
            ]
        );
    }

    #[test]
    fn d_linux_nvidia_with_matching_cudart_selects_cuda() {
        let mut p = HardwareProbe {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            nvidia_gpus: vec![GpuInfo {
                name: "Tesla T4".to_string(),
                vram_bytes: 15360 * crate::local::memory::MIB,
                compute_cap: Some((7, 5)),
            }],
            has_physical_nvidia: true,
            has_usable_nvidia: true,
            driver_cuda_version: Some((13, 0)),
            cuda_runtime_lines: vec![13],
            vulkan_available: true,
            ram_bytes: 32 * crate::local::memory::GIB,
            macos_version: None,
        };
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(chain[0].backend, Backend::Cuda);
        assert_eq!(chain[0].variant.as_deref(), Some("cuda13-older"));

        p.cuda_runtime_lines = vec![12];
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(
            chain[0].backend,
            Backend::Vulkan,
            "major 13 required, only 12 present"
        );
    }

    #[test]
    fn e_cuda_visible_devices_hide_all_forces_cpu_only() {
        let mut p = windows_t4_probe();
        p.has_usable_nvidia = false; // as if CUDA_VISIBLE_DEVICES=-1 was observed by probe()
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(
            chain,
            vec![InstallKind {
                platform: "windows-x64".to_string(),
                backend: Backend::Cpu,
                variant: None,
                cudart: None,
            }]
        );
    }

    #[test]
    fn e_hidden_nvidia_does_not_override_an_explicit_vulkan_pref() {
        // Controller ruling R44 (fix round 1): the hidden-NVIDIA-forces-CPU
        // rule governs automatic selection only. `CUDA_VISIBLE_DEVICES`
        // hiding CUDA devices says nothing about the Vulkan loader/ICD
        // path, and an explicit `backend=vulkan` preference (§20/§21.4)
        // must not silently become a no-op.
        let mut p = windows_t4_probe();
        p.has_usable_nvidia = false;
        let chain = fallback_chain(&p, BackendPref::Vulkan);
        assert_eq!(
            chain.iter().map(|k| k.backend).collect::<Vec<_>>(),
            vec![Backend::Vulkan, Backend::Cpu]
        );
    }

    #[test]
    fn f_macos_arm64_selects_metal_regardless_of_pref() {
        let p = HardwareProbe {
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            nvidia_gpus: vec![],
            has_physical_nvidia: false,
            has_usable_nvidia: false,
            driver_cuda_version: None,
            cuda_runtime_lines: vec![],
            vulkan_available: false,
            ram_bytes: 24 * crate::local::memory::GIB,
            macos_version: Some("15.1".to_string()),
        };
        for pref in [
            BackendPref::Auto,
            BackendPref::Cpu,
            BackendPref::Vulkan,
            BackendPref::Cuda,
        ] {
            assert_eq!(
                fallback_chain(&p, pref),
                vec![InstallKind {
                    platform: "macos-arm64".to_string(),
                    backend: Backend::Metal,
                    variant: None,
                    cudart: None,
                }]
            );
        }
    }

    // --- extra platform coverage --------------------------------------------

    #[test]
    fn g_windows_arm64_is_cpu_only_regardless_of_pref() {
        let p = HardwareProbe {
            os: "windows".to_string(),
            arch: "aarch64".to_string(),
            nvidia_gpus: vec![],
            has_physical_nvidia: false,
            has_usable_nvidia: false,
            driver_cuda_version: None,
            cuda_runtime_lines: vec![],
            vulkan_available: false,
            ram_bytes: 16 * crate::local::memory::GIB,
            macos_version: None,
        };
        assert_eq!(
            fallback_chain(&p, BackendPref::Auto),
            vec![InstallKind {
                platform: "windows-arm64".to_string(),
                backend: Backend::Cpu,
                variant: None,
                cudart: None,
            }]
        );
    }

    // --- coverage gaps from the review: cuda12-{older,newer}, cuda13-portable

    #[test]
    fn selects_cuda12_older_variant() {
        let mut p = windows_t4_probe(); // compute cap 7.5 -> Older bucket
        p.driver_cuda_version = Some((12, 5));
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(chain[0].variant.as_deref(), Some("cuda12-older"));
    }

    #[test]
    fn selects_cuda12_newer_variant() {
        let mut p = windows_t4_probe();
        p.nvidia_gpus = vec![GpuInfo {
            name: "NVIDIA GeForce RTX 5090".to_string(),
            vram_bytes: 32 * crate::local::memory::GIB,
            compute_cap: Some((12, 0)),
        }];
        p.driver_cuda_version = Some((12, 5));
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(chain[0].variant.as_deref(), Some("cuda12-newer"));
    }

    #[test]
    fn selects_cuda13_portable_variant() {
        let mut p = windows_t4_probe();
        p.nvidia_gpus = vec![GpuInfo {
            name: "hypothetical future card".to_string(),
            vram_bytes: 32 * crate::local::memory::GIB,
            compute_cap: Some((15, 0)), // outside every named bucket
        }];
        p.driver_cuda_version = Some((13, 0));
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(chain[0].variant.as_deref(), Some("cuda13-portable"));
    }

    // --- controller ruling R45: platform-aware CUDA variant validity ------

    #[test]
    fn linux_arm64_selects_its_one_shipped_cuda_variant() {
        // linux-arm64 ships exactly one CUDA archive: cuda13-portable.
        // Compute 8.7 (a real Jetson Orin cap) is not in any named bucket,
        // so it lands in Portable already — the realistic case.
        let p = HardwareProbe {
            os: "linux".to_string(),
            arch: "aarch64".to_string(),
            nvidia_gpus: vec![GpuInfo {
                name: "Jetson Orin".to_string(),
                vram_bytes: 8 * crate::local::memory::GIB,
                compute_cap: Some((8, 7)),
            }],
            has_physical_nvidia: true,
            has_usable_nvidia: true,
            driver_cuda_version: Some((13, 0)),
            cuda_runtime_lines: vec![13],
            vulkan_available: true,
            ram_bytes: 16 * crate::local::memory::GIB,
            macos_version: None,
        };
        let k = select_cuda_kind(&p).expect("cuda13-portable is the one arm64 archive");
        assert_eq!(k.platform, "linux-arm64");
        assert_eq!(k.variant.as_deref(), Some("cuda13-portable"));
    }

    #[test]
    fn linux_arm64_falls_back_to_vulkan_when_the_bucket_has_no_arm64_archive() {
        // A Pascal-class card (compute 6.1, "legacy" bucket) would resolve
        // to `cuda12-legacy` on x64 — but linux-arm64 ships no CUDA-12 line
        // at all, only `cuda13-portable`. Before the R45 platform guard,
        // `select_cuda_kind` would have named `cuda12-legacy` on arm64
        // anyway: a real §9 dead stop (DownloadFailed on a nonexistent
        // archive), not a safe degrade.
        let p = HardwareProbe {
            os: "linux".to_string(),
            arch: "aarch64".to_string(),
            nvidia_gpus: vec![GpuInfo {
                name: "old arm64 card".to_string(),
                vram_bytes: 8 * crate::local::memory::GIB,
                compute_cap: Some((6, 1)),
            }],
            has_physical_nvidia: true,
            has_usable_nvidia: true,
            driver_cuda_version: Some((12, 4)),
            cuda_runtime_lines: vec![12],
            vulkan_available: true,
            ram_bytes: 16 * crate::local::memory::GIB,
            macos_version: None,
        };
        assert_eq!(select_cuda_kind(&p), None);
        let chain = fallback_chain(&p, BackendPref::Auto);
        assert_eq!(chain[0].backend, Backend::Vulkan);
    }

    #[test]
    fn cuda12_portable_is_still_selectable_on_linux_x64_and_windows_x64() {
        // Controller ruling R45: cuda12-portable is a REAL shipped archive
        // (verified against the 28 staged assets) and must not be removed.
        // Compute 7.0 (V100) is outside every named bucket -> Portable.
        for (os, platform) in [("linux", "linux-x64"), ("windows", "windows-x64")] {
            let p = HardwareProbe {
                os: os.to_string(),
                arch: "x86_64".to_string(),
                nvidia_gpus: vec![GpuInfo {
                    name: "Tesla V100".to_string(),
                    vram_bytes: 16 * crate::local::memory::GIB,
                    compute_cap: Some((7, 0)),
                }],
                has_physical_nvidia: true,
                has_usable_nvidia: true,
                driver_cuda_version: Some((12, 4)),
                cuda_runtime_lines: vec![12, 13],
                vulkan_available: true,
                ram_bytes: 16 * crate::local::memory::GIB,
                macos_version: None,
            };
            let k = select_cuda_kind(&p).expect("cuda12-portable exists on this platform");
            assert_eq!(k.platform, platform);
            assert_eq!(k.variant.as_deref(), Some("cuda12-portable"));
        }
    }

    #[test]
    fn cuda_selection_never_names_an_archive_outside_the_staged_asset_list() {
        // Ground truth: the 28 staged asset names for tag
        // b10909-mix-bea84f7 (task-2.3-asset-names.md), written out
        // independently of `cuda_variant_exists_on` so this is a real
        // regression check on the production table, not a tautology.
        fn valid_for(platform: &str, variant: &str) -> bool {
            match platform {
                "linux-arm64" => variant == "cuda13-portable",
                "linux-x64" | "windows-x64" => matches!(
                    variant,
                    "cuda12-legacy"
                        | "cuda12-older"
                        | "cuda12-newer"
                        | "cuda12-portable"
                        | "cuda13-older"
                        | "cuda13-newer"
                        | "cuda13-portable"
                ),
                _ => false,
            }
        }

        let caps = [
            (6, 1),
            (7, 5),
            (8, 0),
            (8, 6),
            (8, 9),
            (9, 0),
            (10, 0),
            (12, 0),
            (8, 7), // Jetson Orin, outside every named bucket -> Portable
            (7, 0), // V100, outside every named bucket -> Portable
        ];
        let platforms: [(&str, &str, &str, bool); 3] = [
            ("linux", "x86_64", "linux-x64", true),
            ("windows", "x86_64", "windows-x64", false),
            ("linux", "aarch64", "linux-arm64", true),
        ];

        for (os, arch, platform_name, requires_cudart) in platforms {
            for cap in caps {
                for driver_major in [12u32, 13u32] {
                    let cuda_runtime_lines = if requires_cudart {
                        vec![12, 13]
                    } else {
                        vec![]
                    };
                    let p = HardwareProbe {
                        os: os.to_string(),
                        arch: arch.to_string(),
                        nvidia_gpus: vec![GpuInfo {
                            name: "test-gpu".to_string(),
                            vram_bytes: 8 * crate::local::memory::GIB,
                            compute_cap: Some(cap),
                        }],
                        has_physical_nvidia: true,
                        has_usable_nvidia: true,
                        driver_cuda_version: Some((driver_major, 0)),
                        cuda_runtime_lines,
                        vulkan_available: true,
                        ram_bytes: 16 * crate::local::memory::GIB,
                        macos_version: None,
                    };
                    if let Some(k) = select_cuda_kind(&p) {
                        assert_eq!(k.platform, platform_name);
                        let variant = k.variant.as_deref().unwrap();
                        assert!(
                            valid_for(platform_name, variant),
                            "select_cuda_kind named a nonexistent archive: {platform_name} {variant} (cap {cap:?}, driver {driver_major})"
                        );
                    }
                }
            }
        }
    }

    // --- explicit BackendPref overrides -------------------------------------

    #[test]
    fn explicit_cpu_pref_returns_cpu_only_even_with_a_good_gpu() {
        let p = windows_t4_probe();
        assert_eq!(fallback_chain(&p, BackendPref::Cpu), vec![cpu_kind(&p)]);
    }

    #[test]
    fn explicit_vulkan_pref_returns_vulkan_then_cpu_when_available() {
        let p = windows_t4_probe();
        let chain = fallback_chain(&p, BackendPref::Vulkan);
        assert_eq!(
            chain.iter().map(|k| k.backend).collect::<Vec<_>>(),
            vec![Backend::Vulkan, Backend::Cpu]
        );
    }

    #[test]
    fn explicit_vulkan_pref_without_vulkan_falls_back_to_cpu_only() {
        let mut p = windows_t4_probe();
        p.vulkan_available = false;
        assert_eq!(fallback_chain(&p, BackendPref::Vulkan), vec![cpu_kind(&p)]);
    }

    #[test]
    fn explicit_cuda_pref_without_usable_nvidia_falls_back_to_cpu_only() {
        let p = HardwareProbe {
            os: "windows".to_string(),
            arch: "x86_64".to_string(),
            vulkan_available: true,
            ..Default::default()
        };
        assert_eq!(fallback_chain(&p, BackendPref::Cuda), vec![cpu_kind(&p)]);
    }

    #[test]
    fn explicit_cuda_pref_selects_a_valid_kind_when_cuda_is_usable() {
        // Coverage gap from the review: the `vec![k, cpu_kind(p)]` happy
        // path at the `BackendPref::Cuda` arm was previously only
        // exercised via the fall-to-CPU case above.
        let p = windows_t4_probe();
        let chain = fallback_chain(&p, BackendPref::Cuda);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].backend, Backend::Cuda);
        assert_eq!(chain[0].variant.as_deref(), Some("cuda13-older"));
        assert_eq!(chain[1].backend, Backend::Cpu);
    }

    #[test]
    fn cached_reflects_the_last_value_written_to_the_shared_slot() {
        // Exercises `cached()` without touching real hardware (previously
        // only asserted inside the #[ignore]d live probe test).
        let probe = HardwareProbe {
            os: "test-os".to_string(),
            arch: "test-arch".to_string(),
            ram_bytes: 123,
            ..Default::default()
        };
        {
            let mut guard = CACHED.lock().expect("CACHED mutex poisoned");
            *guard = Some(probe.clone());
        }
        assert_eq!(cached(), Some(probe));
    }

    // --- live probe (this machine) -------------------------------------------

    #[tokio::test]
    #[ignore = "touches real hardware (nvidia-smi/vulkan/cudart/RAM); run explicitly"]
    async fn live_probe_prints_this_machines_hardware() {
        let p = probe().await;
        println!("{p:#?}");
        assert_eq!(cached().as_ref(), Some(&p));

        // This machine is a known quantity (task-2.2-brief.md's verified
        // facts): a Tesla T4 (compute 7.5), a driver reporting CUDA 13.x,
        // and vulkan-1.dll present — so Auto must select `cuda13-older`.
        let chain = fallback_chain(&p, BackendPref::Auto);
        println!("fallback_chain(Auto) = {chain:#?}");
        assert_eq!(chain[0].backend, Backend::Cuda);
        assert_eq!(chain[0].variant.as_deref(), Some("cuda13-older"));
        assert_eq!(chain[0].cudart.as_deref(), Some("13.3"));
    }
}
