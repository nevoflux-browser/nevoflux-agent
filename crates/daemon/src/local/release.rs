//! Pinned engine release constants and the logic that resolves them.
//!
//! The pinned *data* — [`ENGINE_PINNED`], [`ENGINE_COMPATIBLE`] and the
//! staged-asset counts — lives in `release_generated.rs`, produced by
//! `scripts/engine-release/gen_release_rs.py` from a verified local staging
//! manifest (`staged.json`: one entry per asset of one pinned
//! `unslothai/llama.cpp` fork release, actually downloaded and hashed — see
//! `scripts/engine-release/README.md`). Everything else here — the structs,
//! [`mirror_sources`], [`archive_for`] and the tests — is ordinary
//! hand-maintained Rust.
//!
//! That split is deliberate. These definitions and their tests used to live
//! inside a Python string in the generator, where rust-analyzer, clippy,
//! rustfmt and the compiler could not see them, and where a test body could
//! not be checked until it had been generated. Only the data is mechanical,
//! so only the data is generated.
//!
//! Task 2.2's `local::hardware` decides WHICH [`InstallKind`] a machine
//! needs; this module is the pinned table of what that release's downloadable
//! archives actually ARE — name, size, sha256, and (via [`mirror_sources`])
//! where to fetch them from. [`archive_for`] resolves an `InstallKind` to a
//! concrete archive (and, for Windows CUDA, its paired cudart runtime) before
//! downloading.
//!
//! Regenerate the data with:
//! ```text
//! python scripts/engine-release/gen_release_rs.py \
//!     --staged <staging-dir>/staged.json \
//!     --out crates/daemon/src/local/release_generated.rs
//! ```

use crate::local::hardware::{Backend, InstallKind};

/// One downloadable file: its exact name, size, and sha256 (all verified
/// against the real staged download, not guessed).
#[derive(Debug, Clone, Copy)]
pub struct EngineAsset {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// One prebuilt engine archive for a given platform/backend(/variant).
#[derive(Debug, Clone, Copy)]
pub struct EngineArchive {
    /// `"<os>-<arch>"`, e.g. `"windows-x64"` — matches
    /// [`InstallKind::platform`]'s own spelling (`local::hardware::platform_string`),
    /// not the `win`/`windows` split some asset names use.
    pub platform: &'static str,
    pub backend: Backend,
    /// The CUDA archive variant (e.g. `"cuda13-older"`), or `None` for
    /// `Cpu`/`Vulkan`/`Metal`. Matches [`InstallKind::variant`].
    pub variant: Option<&'static str>,
    pub asset: EngineAsset,
}

/// A Windows cudart runtime archive, bundled separately from the app
/// archive it pairs with (see [`archive_for`]).
#[derive(Debug, Clone, Copy)]
pub struct CudartArchive {
    /// The CUDA toolkit version this cudart was built against, e.g.
    /// `"13.3"`.
    pub runtime: &'static str,
    pub asset: EngineAsset,
}

/// One pinned (or historically-compatible) engine release: every archive
/// and cudart runtime it ships, plus its own source tarball.
#[derive(Debug, Clone, Copy)]
pub struct EngineRelease {
    /// The fork (`unslothai/llama.cpp`) release tag this was staged from.
    pub tag: &'static str,
    /// The paired upstream (`ggml-org/llama.cpp`) tag the cudart archives
    /// were published under.
    pub upstream_tag: &'static str,
    pub archives: &'static [EngineArchive],
    pub cudart: &'static [CudartArchive],
    pub source: EngineAsset,
}

/// The GitHub repo every release's assets are mirrored to (ghfast.top and
/// github.com direct, ahead of the fork/upstream original — see
/// [`mirror_sources`]). Published by `scripts/engine-release/publish_mirror.py`
/// (Task 5.1), not by this module.
pub const MIRROR_REPO: &str = "nevoflux-browser/engine-assets";

const FORK_REPO: &str = "unslothai/llama.cpp";
const UPSTREAM_REPO: &str = "ggml-org/llama.cpp";

// The pinned data. `include!` rather than a child module so every existing
// `release::ENGINE_PINNED` path keeps resolving unchanged, and so the data can
// name the structs above without importing them.
include!("release_generated.rs");

/// The three URLs to try, in order, for `asset` of release `tag`: the
/// ghfast.top-fronted mirror, the mirror repo's own GitHub release, then
/// the original fork (or, for a `cudart-`-prefixed asset, upstream) release
/// it was staged from. All three are always genuinely distinct — a caller
/// trying them in order never retries an identical URL.
///
/// `NEVOFLUX_ENGINE_MIRROR_BASE` (tests/dev only), when set, replaces ONLY
/// the first (ghfast-fronted) source with `<base>/<tag>/<asset>` — the
/// mirror repo's own GitHub release and the original fork/upstream source
/// are left real and untouched, since a dev mirror stand-in is not itself
/// authoritative for where the real upstream/fork lives.
pub fn mirror_sources(tag: &str, asset: &str) -> [String; 3] {
    let github = format!("https://github.com/{MIRROR_REPO}/releases/download/engine-{tag}/{asset}");
    let original = original_source_url(tag, asset);
    if let Ok(base) = std::env::var("NEVOFLUX_ENGINE_MIRROR_BASE") {
        let direct = format!("{}/{tag}/{asset}", base.trim_end_matches('/'));
        return [direct, github, original];
    }
    let ghfast = format!("https://ghfast.top/{github}");
    [ghfast, github, original]
}

/// The URL `asset` was originally staged from: `UPSTREAM_REPO` at
/// [`upstream_tag_for`]'s resolved tag for a `cudart-` asset,
/// `FORK_REPO`/`tag` for everything else (cudart archives are an upstream
/// `ggml-org/llama.cpp` artifact — v3 §4.3 — the fork release never
/// contains them).
fn original_source_url(tag: &str, asset: &str) -> String {
    if asset.starts_with("cudart-") {
        let upstream_tag = upstream_tag_for(tag, &ENGINE_PINNED, ENGINE_COMPATIBLE);
        format!("https://github.com/{UPSTREAM_REPO}/releases/download/{upstream_tag}/{asset}")
    } else {
        format!("https://github.com/{FORK_REPO}/releases/download/{tag}/{asset}")
    }
}

/// Resolves `tag`'s paired upstream cudart tag: `pinned.upstream_tag` when
/// `tag` is the pinned release's own tag, else the matching entry's
/// `upstream_tag` in `compatible`, else — only when `tag` matches neither,
/// e.g. a caller probing an unknown/hypothetical tag — a best-effort
/// fallback of the part of `tag` before `-mix-` (the fork's own
/// tag-naming convention, per `mirror_stage.py`).
///
/// **Controller review round 2, Important #1:** that fallback is NOT
/// guaranteed to equal the true upstream tag for a real release — v3 §4.1's
/// own size table records a case where it doesn't (a `b10909` fork line
/// paired with upstream `b10809`, not `b10909`). Always prefer the
/// authoritative `upstream_tag` field (extracted by the generator from the
/// cudart assets' own `source_url`s, not guessed) over re-deriving it by
/// string-splitting; this function exists so nothing downstream of
/// `EngineRelease` ever needs to.
fn upstream_tag_for(tag: &str, pinned: &EngineRelease, compatible: &[EngineRelease]) -> String {
    if tag == pinned.tag {
        return pinned.upstream_tag.to_string();
    }
    if let Some(rel) = compatible.iter().find(|r| r.tag == tag) {
        return rel.upstream_tag.to_string();
    }
    tag.split("-mix-").next().unwrap_or(tag).to_string()
}

/// Resolve `kind` against `rel`'s pinned archive table: the matching
/// [`EngineArchive`], plus its paired [`CudartArchive`] when `kind` names a
/// Windows CUDA variant.
///
/// Matches on `(platform, backend, variant)` only — [`InstallKind::cudart`]
/// is **not** consulted for archive selection; it is informational/
/// roundtrip-only (see its own doc comment). The cudart pairing is instead
/// derived straight from the *matched archive's own* variant string (its
/// `cuda12`/`cuda13` prefix): `local::hardware::known_cudart_version` is
/// deliberately incomplete today (only `cuda13-older` is filled in), and
/// this function must stay authoritative regardless, so that every variant
/// this release actually ships resolves here even before that table
/// catches up. A debug-only consistency check still fires if `kind.cudart`
/// actively contradicts what's derived here — not to change the result,
/// but to surface a `hardware.rs` selector bug producing a false claim.
pub fn archive_for(
    rel: &EngineRelease,
    kind: &InstallKind,
) -> Option<(&'static EngineArchive, Option<&'static CudartArchive>)> {
    let archive = rel.archives.iter().find(|a| {
        a.platform == kind.platform
            && a.backend == kind.backend
            && a.variant == kind.variant.as_deref()
    })?;

    let cudart = if archive.backend == Backend::Cuda && archive.platform.starts_with("windows") {
        let variant = archive.variant?;
        let major_prefix = if variant.starts_with("cuda12") {
            "12"
        } else if variant.starts_with("cuda13") {
            "13"
        } else {
            return None;
        };
        rel.cudart
            .iter()
            .find(|c| c.runtime.starts_with(major_prefix))
    } else {
        None
    };

    debug_assert!(
        match (&kind.cudart, cudart) {
            (Some(claimed), Some(derived)) => claimed.as_str() == derived.runtime,
            _ => true,
        },
        "InstallKind.cudart claims {:?} but archive_for derived {:?} for {:?} -- archive_for's          derivation from the archive's own variant prefix is authoritative (see its doc comment);          this debug_assert exists only to surface a hardware.rs selector bug producing a          contradictory claim, and does not itself change archive_for's result",
        kind.cudart,
        cudart.map(|c| c.runtime),
        kind
    );

    Some((archive, cudart))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::config::BackendPref;
    use crate::local::hardware::{fallback_chain, GpuInfo, HardwareProbe};
    use std::collections::HashSet;

    fn is_hex64(s: &str) -> bool {
        s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
    }

    #[test]
    fn every_archive_and_cudart_sha256_is_64_hex_chars() {
        for a in ENGINE_PINNED.archives {
            assert!(
                is_hex64(a.asset.sha256),
                "{} has a malformed sha256: {}",
                a.asset.name,
                a.asset.sha256
            );
        }
        for c in ENGINE_PINNED.cudart {
            assert!(
                is_hex64(c.asset.sha256),
                "{} has a malformed sha256: {}",
                c.asset.name,
                c.asset.sha256
            );
        }
        assert!(
            is_hex64(ENGINE_PINNED.source.sha256),
            "source asset has a malformed sha256: {}",
            ENGINE_PINNED.source.sha256
        );
    }

    #[test]
    fn windows_cuda13_older_pairs_with_cudart_13_3() {
        // Both the app archive and the cudart archive are verified present
        // in the staged manifest for this exact (platform, variant) pair.
        let kind = InstallKind {
            platform: "windows-x64".to_string(),
            backend: Backend::Cuda,
            variant: Some("cuda13-older".to_string()),
            cudart: Some("13.3".to_string()),
        };
        let (archive, cudart) =
            archive_for(&ENGINE_PINNED, &kind).expect("cuda13-older exists on windows-x64");
        assert_eq!(archive.variant, Some("cuda13-older"));
        assert_eq!(archive.platform, "windows-x64");
        let cudart = cudart.expect("a windows CUDA archive must pair with a cudart runtime");
        assert_eq!(cudart.runtime, "13.3");
    }

    #[test]
    #[should_panic(expected = "InstallKind.cudart claims")]
    fn archive_for_debug_asserts_when_install_kind_cudart_disagrees_with_the_derived_pairing() {
        // Controller review round 2, Minor #4: archive_for ignores
        // kind.cudart for SELECTION (it derives the pairing from the
        // matched archive's own variant prefix instead -- see its doc
        // comment), but a debug-only consistency check must still fire
        // when kind.cudart actively contradicts what's derived, to surface
        // a hardware.rs selector bug rather than silently mask it. This
        // test only runs meaningfully with debug_assertions on, which is
        // the default for both the `dev` and `test` cargo profiles here
        // (no `[profile.dev]`/`[profile.test]` override in this repo).
        let kind = InstallKind {
            platform: "windows-x64".to_string(),
            backend: Backend::Cuda,
            variant: Some("cuda13-older".to_string()),
            cudart: Some("12.4".to_string()), // wrong on purpose: cuda13-older pairs with 13.3
        };
        let _ = archive_for(&ENGINE_PINNED, &kind);
    }

    #[test]
    fn pinned_tag_matches_the_verified_staging_manifest() {
        assert_eq!(ENGINE_PINNED.tag, "b10909-mix-bea84f7");
        assert_eq!(ENGINE_PINNED.upstream_tag, "b10909");
    }

    #[test]
    fn pinned_archive_and_cudart_counts_are_positive_not_just_shaped() {
        // 24 archives + 2 cudart + 1 source + 1
        // skipped manifest = 28 staged assets (ruling R43). A
        // per-row shape assertion (every sha256 is hex, etc.) passes
        // vacuously on an EMPTY table -- this is the actual regression
        // guard for the F8 platform-token trap ("windows" vs "win"), which
        // silently produces an empty table that still compiles.
        assert_eq!(ENGINE_PINNED.archives.len(), 24);
        assert_eq!(ENGINE_PINNED.cudart.len(), 2);
    }

    #[test]
    fn mirror_sources_returns_ghfast_then_github_then_the_original_fork_url() {
        // `mirror_sources` reads `NEVOFLUX_ENGINE_MIRROR_BASE`, so every
        // test that calls it -- not just the one that SETS the var --
        // must hold `ENV_MUTEX` to avoid racing that test's mutation.
        let _env_guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("NEVOFLUX_ENGINE_MIRROR_BASE");
        let asset = "app-b10909-mix-bea84f7-windows-x64-cuda13-older.zip";
        let [ghfast, github, original] = mirror_sources(ENGINE_PINNED.tag, asset);
        let expected_github = format!(
            "https://github.com/{MIRROR_REPO}/releases/download/engine-b10909-mix-bea84f7/{asset}"
        );
        assert_eq!(github, expected_github);
        assert_eq!(ghfast, format!("https://ghfast.top/{github}"));
        assert_eq!(
            original,
            format!("https://github.com/unslothai/llama.cpp/releases/download/b10909-mix-bea84f7/{asset}")
        );
    }

    #[test]
    fn mirror_sources_uses_the_upstream_repo_and_tag_for_a_cudart_asset() {
        let _env_guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("NEVOFLUX_ENGINE_MIRROR_BASE");
        let asset = "cudart-llama-bin-win-cuda-13.3-x64.zip";
        let [_, _, original] = mirror_sources(ENGINE_PINNED.tag, asset);
        assert_eq!(
            original,
            format!("https://github.com/ggml-org/llama.cpp/releases/download/b10909/{asset}")
        );
    }

    #[test]
    fn mirror_sources_env_override_replaces_only_the_first_source() {
        // Shares `llm_gateway::tests::ENV_MUTEX` (the crate's established
        // env-var test-isolation lock -- see `local::sync`'s tests for
        // another non-llm_gateway user of the same lock) rather than
        // inventing a second one. Every other test above that calls
        // `mirror_sources` holds it too, since they'd otherwise race this
        // one's mutation of the same process-global env var.
        let _env_guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                std::env::remove_var("NEVOFLUX_ENGINE_MIRROR_BASE");
            }
        }
        let _restore = EnvGuard;
        std::env::set_var("NEVOFLUX_ENGINE_MIRROR_BASE", "https://example.test/mirror");

        let asset = "cudart-llama-bin-win-cuda-13.3-x64.zip";
        let [direct, github, original] = mirror_sources(ENGINE_PINNED.tag, asset);
        assert_eq!(
            direct,
            format!("https://example.test/mirror/b10909-mix-bea84f7/{asset}")
        );
        // The mirror repo's own GitHub release and the original upstream
        // URL are both untouched by the override (controller review round
        // 2, Minor #3: previously the first TWO entries were both the
        // override, so a caller trying them in order retried an identical
        // URL before ever reaching a real fallback).
        assert_eq!(
            github,
            format!("https://github.com/{MIRROR_REPO}/releases/download/engine-b10909-mix-bea84f7/{asset}")
        );
        assert_eq!(
            original,
            format!("https://github.com/ggml-org/llama.cpp/releases/download/b10909/{asset}")
        );
        assert_ne!(direct, github, "all three sources must be distinct");
        assert_ne!(github, original, "all three sources must be distinct");
        assert_ne!(direct, original, "all three sources must be distinct");
    }

    #[test]
    fn upstream_tag_for_uses_the_authoritative_field_not_a_naive_tag_split() {
        // Regression for controller review round 2, Important #1: a fork
        // tag like "b10909-mix-bea84f7" naively splits (on "-mix-") to
        // "b10909", but nothing guarantees the paired upstream cudart tag
        // equals that -- v3 §4.1's own size table records a case where it
        // doesn't (a b10909 fork line paired with upstream b10809, not
        // b10909). A synthetic release makes that disagreement concrete,
        // rather than relying on the real pinned tag, where the split and
        // the authoritative field happen to coincide.
        let synthetic = EngineRelease {
            tag: "b10909-mix-deadbeef",
            upstream_tag: "b10809",
            archives: &[],
            cudart: &[],
            source: EngineAsset {
                name: "x",
                bytes: 0,
                sha256: "x",
            },
        };
        assert_eq!(
            upstream_tag_for(synthetic.tag, &synthetic, &[]),
            "b10809",
            "must use the authoritative upstream_tag field, not tag.split(\"-mix-\")"
        );

        // The real pinned release still resolves correctly (here the split
        // and the field happen to agree, which is exactly why the
        // synthetic case above is the one that actually locks this down).
        assert_eq!(
            upstream_tag_for(ENGINE_PINNED.tag, &ENGINE_PINNED, ENGINE_COMPATIBLE),
            ENGINE_PINNED.upstream_tag
        );

        // A tag matching neither the pinned nor any compatible release
        // falls back to the best-effort split -- documented as possibly
        // wrong for a real release, but the only option left for an
        // unrecognized tag.
        assert_eq!(
            upstream_tag_for("totally-unknown-mix-tag", &ENGINE_PINNED, ENGINE_COMPATIBLE),
            "totally-unknown"
        );
    }

    /// Ruling R45 cross-check: every `InstallKind` `local::hardware` can
    /// actually return (driven through its real, public `fallback_chain`,
    /// across every platform / compute-cap bucket / driver-major
    /// combination its own 60-case regression test walks -- NOT a
    /// hand-copied re-listing of `cuda_variant_exists_on`'s match arms)
    /// must resolve to `Some(..)` here. This is the only place the probe's
    /// hand-written platform table meets the real 28-asset
    /// release table; without it, a future tag change could silently
    /// reopen the v3 §9 dead-stop hole where a selected (platform, kind)
    /// has no matching asset and the install stops dead instead of
    /// degrading to the next tier.
    #[test]
    fn every_hardware_probe_install_kind_resolves_to_a_pinned_archive() {
        let gib = crate::local::memory::GIB;

        struct PlatformCase {
            os: &'static str,
            arch: &'static str,
            needs_cuda_runtime_line: bool,
        }
        let platforms = [
            PlatformCase {
                os: "linux",
                arch: "x86_64",
                needs_cuda_runtime_line: true,
            },
            PlatformCase {
                os: "linux",
                arch: "aarch64",
                needs_cuda_runtime_line: true,
            },
            PlatformCase {
                os: "windows",
                arch: "x86_64",
                needs_cuda_runtime_line: false,
            },
            PlatformCase {
                os: "windows",
                arch: "aarch64",
                needs_cuda_runtime_line: false,
            },
            PlatformCase {
                os: "macos",
                arch: "x86_64",
                needs_cuda_runtime_line: false,
            },
            PlatformCase {
                os: "macos",
                arch: "aarch64",
                needs_cuda_runtime_line: false,
            },
        ];
        // Same compute-cap buckets and driver majors as
        // `hardware::tests::cuda_selection_never_names_an_archive_outside_the_staged_asset_list`.
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
                    let cuda_runtime_lines = if pc.needs_cuda_runtime_line {
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

        // Exact, not a loose floor (controller review round 2, Minor #2):
        // the probe matrix reaches exactly one distinct InstallKind per
        // pinned archive (24 today) -- a `>= 10` floor would still pass if
        // an entire CUDA major line (8 kinds) silently dropped out of
        // `fallback_chain`, leaving 16, and would never notice that
        // `cuda12-portable` -- the exact variant R45 exists to protect --
        // stopped being reachable.
        assert_eq!(
            kinds.len(),
            ENGINE_PINNED.archives.len(),
            "expected exactly one distinct InstallKind per pinned archive from the probe matrix, \
             got {} kinds for {} archives: {kinds:#?}",
            kinds.len(),
            ENGINE_PINNED.archives.len()
        );

        for kind in &kinds {
            assert!(
                archive_for(&ENGINE_PINNED, kind).is_some(),
                "local::hardware::fallback_chain can return {kind:?} but archive_for finds no \
                 matching archive in ENGINE_PINNED -- the release table and the hardware probe's \
                 platform table have drifted apart"
            );
        }
    }
}
