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
FILE_TEMPLATE = '''// Pinned engine release data — GENERATED by
// `scripts/engine-release/gen_release_rs.py`, do not hand-edit.
//
// Included textually by `release.rs`, which owns the struct definitions, the
// resolution logic and the tests. Only the mechanical part — the table staged
// from a verified `staged.json` — is generated.

/// Staged assets this table was generated from, in total.
///
/// `STAGED_ARCHIVE_COUNT` + `STAGED_CUDART_COUNT` + 1 source + 1 skipped
/// manifest. `release.rs`'s tests assert the archive and cudart counts against
/// hand-written literals rather than against these constants: two generated
/// values agreeing with each other proves nothing, whereas a literal a human
/// must update is a real pin on a re-pin.
pub const STAGED_ASSET_COUNT: usize = @@TOTAL_COUNT@@;
/// [`EngineArchive`]s in [`ENGINE_PINNED`].
pub const STAGED_ARCHIVE_COUNT: usize = @@ARCHIVE_COUNT@@;
/// [`CudartArchive`]s in [`ENGINE_PINNED`].
pub const STAGED_CUDART_COUNT: usize = @@CUDART_COUNT@@;

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
'''


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

    rendered = FILE_TEMPLATE
    for token, value in (
        ("@@TOTAL_COUNT@@", str(total_count)),
        ("@@ARCHIVE_COUNT@@", str(archive_count)),
        ("@@CUDART_COUNT@@", str(cudart_count)),
        ("@@TAG_LIT@@", tag_lit),
        ("@@UPSTREAM_TAG_LIT@@", upstream_tag_lit),
        ("@@ARCHIVES_RS@@", archives_rs),
        ("@@CUDART_RS@@", cudart_rs),
        ("@@SOURCE_RS@@", source_rs),
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
