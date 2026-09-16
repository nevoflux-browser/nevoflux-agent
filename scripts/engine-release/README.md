# Engine release tooling

This directory pins one `unslothai/llama.cpp` fork release (the on-device
`llama-server` build NevoFlux bundles) into
[`crates/daemon/src/local/release.rs`](../../crates/daemon/src/local/release.rs),
and mirrors that release's assets to a GitHub repo NevoFlux controls, so an
install never depends solely on the fork's own release staying up.

## The three pieces

1. **`local::hardware`** (Task 2.2, not in this directory) decides WHICH
   `InstallKind` — platform + backend + CUDA variant — a machine needs.
2. **`local::release`** (this directory generates it) is the pinned table of
   what that release's downloadable archives actually ARE: name, size,
   sha256, and (via `mirror_sources`) where to download them from.
   `archive_for(rel, kind)` resolves an `InstallKind` to a concrete archive
   (and, for Windows CUDA, its paired cudart runtime).
3. A later task's installer calls `archive_for` + `mirror_sources` to
   actually download and verify an archive before launching the engine.

## Workflow

### 1. Stage the release

Staging (downloading every asset of one fork release, plus its paired
upstream cudart runtimes, and hashing each file) happens OUTSIDE this repo,
in a local staging directory — see that directory's own `mirror_stage.py`
for how `staged.json` gets built. `staged.json` is a JSON object keyed by
asset name:

```json
{
  "<asset-name>": {
    "bytes": 12345,
    "sha256": "<64 hex chars>",
    "source_url": "https://github.com/<owner>/<repo>/releases/download/<tag>/<asset-name>",
    "verified_by": "fork-sha256-json"
  },
  ...
}
```

A pinned release stages exactly 28 assets: 24 platform/backend/variant app
archives + 2 Windows cudart runtime archives + 1 source tarball + 1 fork
build-metadata manifest (`llama-prebuilt-manifest.json`, uploaded to the
mirror for byte-parity with the fork but not represented in `EngineRelease`
— see ruling R43 below).

### 2. Generate `release.rs`

```bash
python scripts/engine-release/gen_release_rs.py \
    --staged <staging-dir>/staged.json \
    --out crates/daemon/src/local/release.rs
```

Everything in the generated file is derived from `staged.json`'s own
content — the fork tag from the `llama.cpp-source-<tag>.tar.gz` singleton's
own name, the upstream tag from the cudart assets' own `source_url`s, every
archive's platform/backend/variant from its asset name. Nothing about a
specific release is hand-typed into the generator script.

After regenerating, format it (this repo has no `.gitattributes` and mixed
line endings — never run a bare `cargo fmt`, which would rewrite unrelated
files):

```bash
rustfmt --edition 2021 crates/daemon/src/local/release.rs
```

Then run its test module:

```bash
CARGO_PROFILE_TEST_DEBUG=0 cargo test -p nevoflux-daemon --lib -j 3 local::release::
```

One of those tests (`every_hardware_probe_install_kind_resolves_to_a_pinned_archive`)
cross-checks the regenerated table against `local::hardware`'s real,
public `fallback_chain` output across every platform / compute-cap bucket /
driver-major combination — the only place the hardware probe's own platform
table meets the actual release asset list. If a future tag drops or renames
an archive `local::hardware` still expects, this test catches it before an
install can silently dead-stop instead of degrading to the next backend
tier (v3 §9).

### 3. Mirror the release's assets

```bash
python scripts/engine-release/publish_mirror.py \
    --staged <staging-dir>/staged.json \
    --repo nevoflux-browser/engine-assets \
    --tag engine-<fork-tag>
```

**Dry-run by default, and fully offline by default.** Without `--execute`,
this script never calls `gh release create`/`gh release upload`, and by
default it makes no `gh` calls at all (not even read-only ones) — it just
prints the commands it would run for both possible cases (release exists /
doesn't exist yet), so it's safe to run with no network access. Pass
`--check-remote` to additionally run two read-only `gh` calls (`gh repo
view` / `gh release view`) that narrow the output down to the one case that
actually applies. Pass `--execute` only once publishing is actually
authorized (Task 5.1) — it needs `gh` authenticated with write access to
`--repo`, and always runs the same read-only check first (it needs to know
whether to create or upload `--clobber`).

The mirror tag is `engine-<fork-tag>` (e.g. `engine-b10909-mix-bea84f7`),
matching what `local::release::mirror_sources` constructs when building the
ghfast.top/github.com mirror URLs for that release's `tag`.

All 28 staged assets are uploaded, including `llama-prebuilt-manifest.json`
— `gen_release_rs.py` skips it for `EngineRelease` (ruling R43: it's fork
build metadata with no matching field), but the mirror stays byte-faithful
with the fork's own release regardless.

## Five recognized asset-name patterns

`gen_release_rs.py` recognizes exactly five asset-name shapes (four map to
a field of `EngineRelease`; the fifth is a deliberate skip, not a mapping —
see ruling R43 below); anything else is a hard generation error (never a
silently-dropped asset):

| Pattern | Maps to |
| --- | --- |
| `app-<tag>-<platform>-<backend>.tar.gz\|.zip` | `EngineArchive` (`Cpu`/`Vulkan`/`Cuda`) |
| `llama-<tag>-bin-macos-<arch>.tar.gz` | `EngineArchive` (`Metal`) |
| `cudart-llama-bin-win-cuda-<ver>-x64.zip` | `CudartArchive` |
| `llama.cpp-source-<tag>.tar.gz` | `EngineRelease::source` |
| `llama-prebuilt-manifest.json` | skipped (see ruling R43 above) |

**Platform token trap (F8):** app archives spell the Windows platform
`windows` (`app-...-windows-x64-...`), while cudart archives spell it `win`
(`cudart-llama-bin-win-cuda-...`). The generator matches both spellings
explicitly — a parser that only recognized one would silently produce an
*empty* archive table for the other family, and that table would still
compile and still pass per-row shape assertions (every field is well-typed,
there just aren't any rows). This is exactly why `release.rs`'s test module
asserts a positive `ENGINE_PINNED.archives.len()` / `.cudart.len()`, not
just that whatever rows exist look correct.

## `ENGINE_COMPATIBLE`

`EngineRelease` supports pinning more than one release — `ENGINE_PINNED` is
the one currently installed by default; `ENGINE_COMPATIBLE` lists older
releases still accepted for an existing install, so a machine already
running an older engine build isn't forced to redownload on every daemon
update. It's empty for this, the first pinned tag; a future re-pin should
generate the new `ENGINE_PINNED` and hand-move the previous one into
`ENGINE_COMPATIBLE` (the generator only ever produces one release's table
per run).
