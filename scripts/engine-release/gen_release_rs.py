#!/usr/bin/env python3
"""Generate `crates/daemon/src/local/release.rs` from a verified staging manifest.

Task 2.2 (`local::hardware`) decided WHICH `InstallKind` (platform + backend +
variant) a machine needs. This script turns a `staged.json` — produced by
staging every asset of one pinned `unslothai/llama.cpp` fork release (plus its
paired upstream `ggml-org/llama.cpp` cudart runtimes) and hashing each file —
into the pinned Rust constant table of what those archives actually ARE: name,
size, sha256. Everything here is derived from `staged.json`'s own content
(asset names as dict keys, `source_url` for the fork/upstream tags); nothing
about the current release is hand-typed into this script.

`staged.json` shape (one top-level JSON object, ASSET NAME IS THE DICT KEY):
    {
      "<asset-name>": {"bytes": int, "sha256": "<64 hex chars>",
                        "source_url": "https://...", "verified_by": "..."},
      ...
    }

Five asset-name patterns are recognized (four map to a field of
EngineRelease; the fifth is a deliberate skip, not a mapping):
  - `app-<tag>-<platform>-<backend>.tar.gz|.zip`            -> EngineArchive
  - `llama-<tag>-bin-macos-<arch>.tar.gz`                   -> EngineArchive (Metal)
  - `cudart-llama-bin-win-cuda-<ver>-x64.zip`                -> CudartArchive
  - `llama.cpp-source-<tag>.tar.gz`                          -> EngineRelease::source
  - `llama-prebuilt-manifest.json`                           -> skipped (fork
    build metadata; no field of `EngineRelease` holds it, and
    `publish_mirror.py` uploads it separately so the mirror stays
    byte-faithful with the fork's own release).

Usage:
    python gen_release_rs.py --staged <staging-dir>/staged.json \\
        --out crates/daemon/src/local/release.rs
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Optional

# The GitHub repo this engine's release assets are mirrored to (Task 5.1
# publishes there; this is a fixed project constant, not derived from any
# one release's staging data).
MIRROR_REPO = "nevoflux-browser/engine-assets"

MANIFEST_ASSET_NAME = "llama-prebuilt-manifest.json"

APP_PLATFORMS = ["linux-arm64", "linux-x64", "windows-arm64", "windows-x64"]

CUDART_RE = re.compile(r"^cudart-llama-bin-win-cuda-([0-9.]+)-x64\.zip$")
SOURCE_URL_TAG_RE = re.compile(r"/releases/download/([^/]+)/")


class GenError(SystemExit):
    """Raised (as SystemExit) for any staged.json content that doesn't match
    one of the verified naming schemes — fail loudly rather than silently
    dropping an asset (that's exactly how the F8 platform-token trap hides:
    an unmatched asset that quietly becomes an empty table entry instead of
    a hard error)."""


def load_staged(path: Path) -> dict:
    with path.open(encoding="utf-8") as f:
        data = json.load(f)
    if not isinstance(data, dict) or not data:
        raise GenError(f"{path}: expected a non-empty JSON object keyed by asset name")
    for name, meta in data.items():
        for field in ("bytes", "sha256", "source_url"):
            if field not in meta:
                raise GenError(f"{path}: asset {name!r} is missing required field {field!r}")
    return data


def url_tag(source_url: str) -> str:
    m = SOURCE_URL_TAG_RE.search(source_url)
    if not m:
        raise GenError(f"cannot find a /releases/download/<tag>/ segment in {source_url!r}")
    return m.group(1)


def derive_tag(staged: dict) -> str:
    """The fork release tag, read from the `llama.cpp-source-<tag>.tar.gz`
    singleton's own name (unambiguous: no other name segment could be
    mistaken for it)."""
    prefix, suffix = "llama.cpp-source-", ".tar.gz"
    for name in staged:
        if name.startswith(prefix) and name.endswith(suffix):
            return name[len(prefix) : -len(suffix)]
    raise GenError(
        "no llama.cpp-source-<tag>.tar.gz singleton found in staged.json; cannot derive the release tag"
    )


def derive_upstream_tag(staged: dict, tag: str) -> str:
    """The upstream `ggml-org/llama.cpp` tag the cudart archives were
    published under, read from their own `source_url`s (verified against the
    real staging manifest, not guessed from the fork tag's own spelling)."""
    tags = {url_tag(meta["source_url"]) for name, meta in staged.items() if name.startswith("cudart-")}
    if not tags:
        raise GenError("no cudart-* assets found in staged.json")
    if len(tags) > 1:
        raise GenError(f"cudart assets disagree on their upstream release tag: {sorted(tags)}")
    upstream_tag = tags.pop()
    fork_tags = {
        url_tag(meta["source_url"])
        for name, meta in staged.items()
        if not name.startswith("cudart-")
    }
    if fork_tags != {tag}:
        raise GenError(
            f"non-cudart assets' source_url tags {sorted(fork_tags)} don't all match the "
            f"derived fork tag {tag!r}"
        )
    return upstream_tag


def parse_app_asset(name: str, tag: str) -> Optional[tuple[str, str]]:
    """`app-<tag>-<platform>-<backend>.tar.gz|.zip` -> (platform, backend_token)."""
    prefix = f"app-{tag}-"
    if not name.startswith(prefix):
        return None
    rest = name[len(prefix) :]
    for ext in (".tar.gz", ".zip"):
        if rest.endswith(ext):
            rest = rest[: -len(ext)]
            break
    else:
        raise GenError(f"app asset {name!r} has neither a .tar.gz nor a .zip extension")
    # Platform token trap (F8): match the platform prefix explicitly rather
    # than splitting on a fixed number of hyphens — the backend token itself
    # contains hyphens (e.g. "cuda12-legacy"), so a naive split miscounts.
    for platform in APP_PLATFORMS:
        if rest == platform or rest.startswith(platform + "-"):
            backend_token = rest[len(platform) :].lstrip("-")
            if not backend_token:
                raise GenError(f"app asset {name!r} has a platform but no backend token")
            return platform, backend_token
    raise GenError(f"app asset {name!r}: {rest!r} does not start with a known platform ({APP_PLATFORMS})")


def backend_and_variant(backend_token: str) -> tuple[str, Optional[str]]:
    if backend_token == "cpu":
        return "Cpu", None
    if backend_token == "vulkan":
        return "Vulkan", None
    if backend_token.startswith("cuda"):
        return "Cuda", backend_token
    raise GenError(f"unrecognized backend token {backend_token!r}")


def parse_macos_asset(name: str, tag: str) -> Optional[str]:
    """`llama-<tag>-bin-macos-<arch>.tar.gz` -> platform string, e.g. "macos-arm64"."""
    prefix, suffix = f"llama-{tag}-bin-macos-", ".tar.gz"
    if not (name.startswith(prefix) and name.endswith(suffix)):
        return None
    arch = name[len(prefix) : -len(suffix)]
    if arch not in ("arm64", "x64"):
        raise GenError(f"macos asset {name!r} has an unrecognized arch {arch!r}")
    return f"macos-{arch}"


def classify(staged: dict, tag: str):
    """Sort every staged asset into archives / cudart / source / skipped.
    Raises if any asset name matches none of the four known schemes."""
    archives = []  # (platform, backend, variant, name, meta)
    cudart = []  # (runtime, name, meta)
    source = None  # (name, meta)
    skipped = []

    for name, meta in staged.items():
        if name == MANIFEST_ASSET_NAME:
            skipped.append(name)
            continue

        if name == f"llama.cpp-source-{tag}.tar.gz":
            if source is not None:
                raise GenError(f"more than one source asset matched: {source[0]!r} and {name!r}")
            source = (name, meta)
            continue

        m = CUDART_RE.match(name)
        if m:
            cudart.append((m.group(1), name, meta))
            continue

        macos_platform = parse_macos_asset(name, tag)
        if macos_platform:
            archives.append((macos_platform, "Metal", None, name, meta))
            continue

        app = parse_app_asset(name, tag)
        if app:
            platform, backend_token = app
            backend, variant = backend_and_variant(backend_token)
            archives.append((platform, backend, variant, name, meta))
            continue

        raise GenError(f"asset {name!r} matched none of the four known naming schemes")

    if source is None:
        raise GenError(f"no llama.cpp-source-{tag}.tar.gz asset found")

    backend_rank = {"Cpu": 0, "Vulkan": 1, "Cuda": 2, "Metal": 3}
    archives.sort(key=lambda a: (a[0], backend_rank[a[1]], a[2] or ""))
    cudart.sort(key=lambda c: c[0])

    return archives, cudart, source, skipped


def rust_str(s: str) -> str:
    return json.dumps(s)


def rust_option_str(s: Optional[str]) -> str:
    return "None" if s is None else f"Some({rust_str(s)})"


def render_asset(name: str, meta: dict, field_indent: str) -> str:
    """Renders `EngineAsset { ... }` with its OPENING brace unindented (the
    caller's own prefix, e.g. "asset: ", precedes it) but its fields and
    closing brace properly indented under `field_indent`."""
    closing_indent = field_indent[:-4] if len(field_indent) >= 4 else ""
    return (
        "EngineAsset {\n"
        f"{field_indent}name: {rust_str(name)},\n"
        f"{field_indent}bytes: {meta['bytes']},\n"
        f"{field_indent}sha256: {rust_str(meta['sha256'])},\n"
        f"{closing_indent}}}"
    )


def render_archives(archives) -> str:
    lines = []
    for platform, backend, variant, name, meta in archives:
        lines.append("        EngineArchive {")
        lines.append(f"            platform: {rust_str(platform)},")
        lines.append(f"            backend: Backend::{backend},")
        lines.append(f"            variant: {rust_option_str(variant)},")
        lines.append(f"            asset: {render_asset(name, meta, ' ' * 16)},")
        lines.append("        },")
    return "\n".join(lines)


def render_cudart(cudart) -> str:
    lines = []
    for runtime, name, meta in cudart:
        lines.append("        CudartArchive {")
        lines.append(f"            runtime: {rust_str(runtime)},")
        lines.append(f"            asset: {render_asset(name, meta, ' ' * 16)},")
        lines.append("        },")
    return "\n".join(lines)


# Placeholders below use @@TOKEN@@ markers (never brace-based .format()/f-string
# substitution) precisely because the surrounding text IS Rust source, which is
# saturated with literal `{`/`}` — doubling every one of them for str.format()
# is exactly the kind of thing that silently breaks (as it did during
# development of this script) without a syntax error pointing at the cause.
TEST_MODULE_TEMPLATE = '''
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
        assert_eq!(ENGINE_PINNED.tag, @@TAG_LIT@@);
        assert_eq!(ENGINE_PINNED.upstream_tag, @@UPSTREAM_TAG_LIT@@);
    }

    #[test]
    fn pinned_archive_and_cudart_counts_are_positive_not_just_shaped() {
        // @@ARCHIVE_COUNT@@ archives + @@CUDART_COUNT@@ cudart + 1 source + 1
        // skipped manifest = @@TOTAL_COUNT@@ staged assets (ruling R43). A
        // per-row shape assertion (every sha256 is hex, etc.) passes
        // vacuously on an EMPTY table -- this is the actual regression
        // guard for the F8 platform-token trap ("windows" vs "win"), which
        // silently produces an empty table that still compiles.
        assert_eq!(ENGINE_PINNED.archives.len(), @@ARCHIVE_COUNT@@);
        assert_eq!(ENGINE_PINNED.cudart.len(), @@CUDART_COUNT@@);
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
        let asset = "app-@@TAG@@-windows-x64-cuda13-older.zip";
        let [ghfast, github, original] = mirror_sources(ENGINE_PINNED.tag, asset);
        let expected_github = format!(
            "https://github.com/{MIRROR_REPO}/releases/download/engine-@@TAG@@/{asset}"
        );
        assert_eq!(github, expected_github);
        assert_eq!(ghfast, format!("https://ghfast.top/{github}"));
        assert_eq!(
            original,
            format!("https://github.com/unslothai/llama.cpp/releases/download/@@TAG@@/{asset}")
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
            format!(
                "https://github.com/ggml-org/llama.cpp/releases/download/@@UPSTREAM_TAG@@/{asset}"
            )
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
        assert_eq!(direct, format!("https://example.test/mirror/@@TAG@@/{asset}"));
        // The mirror repo's own GitHub release and the original upstream
        // URL are both untouched by the override (controller review round
        // 2, Minor #3: previously the first TWO entries were both the
        // override, so a caller trying them in order retried an identical
        // URL before ever reaching a real fallback).
        assert_eq!(
            github,
            format!("https://github.com/{MIRROR_REPO}/releases/download/engine-@@TAG@@/{asset}")
        );
        assert_eq!(
            original,
            format!(
                "https://github.com/ggml-org/llama.cpp/releases/download/@@UPSTREAM_TAG@@/{asset}"
            )
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
            source: EngineAsset { name: "x", bytes: 0, sha256: "x" },
        };
        assert_eq!(
            upstream_tag_for(synthetic.tag, &synthetic, &[]),
            "b10809",
            "must use the authoritative upstream_tag field, not tag.split(\\"-mix-\\")"
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
    /// hand-written platform table meets the real @@TOTAL_COUNT@@-asset
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
            PlatformCase { os: "linux", arch: "x86_64", needs_cuda_runtime_line: true },
            PlatformCase { os: "linux", arch: "aarch64", needs_cuda_runtime_line: true },
            PlatformCase { os: "windows", arch: "x86_64", needs_cuda_runtime_line: false },
            PlatformCase { os: "windows", arch: "aarch64", needs_cuda_runtime_line: false },
            PlatformCase { os: "macos", arch: "x86_64", needs_cuda_runtime_line: false },
            PlatformCase { os: "macos", arch: "aarch64", needs_cuda_runtime_line: false },
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
            "expected exactly one distinct InstallKind per pinned archive from the probe matrix, \\
             got {} kinds for {} archives: {kinds:#?}",
            kinds.len(),
            ENGINE_PINNED.archives.len()
        );

        for kind in &kinds {
            assert!(
                archive_for(&ENGINE_PINNED, kind).is_some(),
                "local::hardware::fallback_chain can return {kind:?} but archive_for finds no \\
                 matching archive in ENGINE_PINNED -- the release table and the hardware probe's \\
                 platform table have drifted apart"
            );
        }
    }
}
'''

FILE_TEMPLATE = '''//! Pinned engine release constants — GENERATED, do not hand-edit.
//!
//! Produced by `scripts/engine-release/gen_release_rs.py` from a verified
//! local staging manifest (`staged.json`: one entry per asset of one pinned
//! `unslothai/llama.cpp` fork release, actually downloaded and hashed —
//! see `scripts/engine-release/README.md`). Task 2.2's `local::hardware`
//! decides WHICH [`InstallKind`] a machine needs; this module is the pinned
//! table of what that release's downloadable archives actually ARE — name,
//! size, sha256, and (via [`mirror_sources`]) where to fetch them from. A
//! later task consumes [`archive_for`] to resolve an `InstallKind` to a
//! concrete archive (and, for Windows CUDA, its paired cudart runtime)
//! before downloading.
//!
//! Regenerate with:
//! ```text
//! python scripts/engine-release/gen_release_rs.py \\
//!     --staged <staging-dir>/staged.json \\
//!     --out crates/daemon/src/local/release.rs
//! ```
//!
//! `@@TOTAL_COUNT@@` staged assets account for exactly `@@ARCHIVE_COUNT@@`
//! [`EngineArchive`]s + `@@CUDART_COUNT@@` [`CudartArchive`]s + 1
//! [`EngineRelease::source`] + 1 skipped asset
//! (`llama-prebuilt-manifest.json`: fork build metadata with no field of
//! `EngineRelease` to hold it — `scripts/engine-release/publish_mirror.py`
//! still uploads it so the mirror stays byte-faithful with the fork's own
//! release).

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
pub const MIRROR_REPO: &str = "@@MIRROR_REPO@@";

const FORK_REPO: &str = "unslothai/llama.cpp";
const UPSTREAM_REPO: &str = "ggml-org/llama.cpp";

/// The engine release this daemon currently installs.
pub const ENGINE_PINNED: EngineRelease = EngineRelease {
    tag: @@TAG_LIT@@,
    upstream_tag: @@UPSTREAM_TAG_LIT@@,
    archives: &[
@@ARCHIVES_RS@@
    ],
    cudart: &[
@@CUDART_RS@@
    ],
    source: @@SOURCE_RS@@,
};

/// Older pinned releases still accepted for an existing install (so a
/// machine already running an older engine isn't forced to redownload on
/// every daemon update). Empty for this, the first pinned tag.
pub const ENGINE_COMPATIBLE: &[EngineRelease] = &[];

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
    let github = format!(
        "https://github.com/{MIRROR_REPO}/releases/download/engine-{tag}/{asset}"
    );
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
        a.platform == kind.platform && a.backend == kind.backend && a.variant == kind.variant.as_deref()
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
        rel.cudart.iter().find(|c| c.runtime.starts_with(major_prefix))
    } else {
        None
    };

    debug_assert!(
        match (&kind.cudart, cudart) {
            (Some(claimed), Some(derived)) => claimed.as_str() == derived.runtime,
            _ => true,
        },
        "InstallKind.cudart claims {:?} but archive_for derived {:?} for {:?} -- archive_for's \
         derivation from the archive's own variant prefix is authoritative (see its doc comment); \
         this debug_assert exists only to surface a hardware.rs selector bug producing a \
         contradictory claim, and does not itself change archive_for's result",
        kind.cudart,
        cudart.map(|c| c.runtime),
        kind
    );

    Some((archive, cudart))
}
@@TEST_MODULE@@'''


def render_file(tag: str, upstream_tag: str, archives, cudart, source) -> str:
    archive_count = len(archives)
    cudart_count = len(cudart)
    total_count = archive_count + cudart_count + 2  # + source + skipped manifest

    source_name, source_meta = source
    tag_lit = rust_str(tag)
    upstream_tag_lit = rust_str(upstream_tag)
    archives_rs = render_archives(archives)
    cudart_rs = render_cudart(cudart)
    source_rs = render_asset(source_name, source_meta, " " * 8)

    test_module = TEST_MODULE_TEMPLATE
    for token, value in (
        ("@@TAG@@", tag),
        ("@@TAG_LIT@@", tag_lit),
        ("@@UPSTREAM_TAG@@", upstream_tag),
        ("@@UPSTREAM_TAG_LIT@@", upstream_tag_lit),
        ("@@ARCHIVE_COUNT@@", str(archive_count)),
        ("@@CUDART_COUNT@@", str(cudart_count)),
        ("@@TOTAL_COUNT@@", str(total_count)),
    ):
        test_module = test_module.replace(token, value)

    rendered = FILE_TEMPLATE
    for token, value in (
        ("@@TOTAL_COUNT@@", str(total_count)),
        ("@@ARCHIVE_COUNT@@", str(archive_count)),
        ("@@CUDART_COUNT@@", str(cudart_count)),
        ("@@MIRROR_REPO@@", MIRROR_REPO),
        ("@@TAG_LIT@@", tag_lit),
        ("@@UPSTREAM_TAG_LIT@@", upstream_tag_lit),
        ("@@ARCHIVES_RS@@", archives_rs),
        ("@@CUDART_RS@@", cudart_rs),
        ("@@SOURCE_RS@@", source_rs),
        ("@@TEST_MODULE@@", test_module),
    ):
        rendered = rendered.replace(token, value)
    return rendered


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--staged", required=True, type=Path, help="path to staged.json")
    ap.add_argument("--out", required=True, type=Path, help="output path, e.g. crates/daemon/src/local/release.rs")
    args = ap.parse_args()

    staged = load_staged(args.staged)
    tag = derive_tag(staged)
    upstream_tag = derive_upstream_tag(staged, tag)
    archives, cudart, source, skipped = classify(staged, tag)

    total = len(archives) + len(cudart) + 1 + len(skipped)
    if total != len(staged):
        raise GenError(
            f"accounting mismatch: {len(archives)} archives + {len(cudart)} cudart + 1 source + "
            f"{len(skipped)} skipped = {total}, but staged.json has {len(staged)} assets"
        )

    print(f"tag={tag} upstream_tag={upstream_tag}", file=sys.stderr)
    print(
        f"{len(archives)} archives, {len(cudart)} cudart, 1 source, {len(skipped)} skipped "
        f"({', '.join(skipped)}) -- {len(staged)} staged assets total",
        file=sys.stderr,
    )

    rendered = render_file(tag, upstream_tag, archives, cudart, source)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(rendered, encoding="utf-8", newline="\n")
    print(f"wrote {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
