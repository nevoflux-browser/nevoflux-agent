#!/usr/bin/env python3
"""Mirror a staged engine release to the nevoflux-browser/engine-assets repo.

Uploads every asset listed in `staged.json` — all 28, including
`llama-prebuilt-manifest.json` (which `gen_release_rs.py` skips for
`EngineRelease`, since no field of it holds fork build metadata — see ruling
R43) — as one GitHub release, so `local::release::mirror_sources`'s
ghfast.top/github.com mirror URLs resolve.

**Dry-run by default.** Without --execute, this script never calls `gh
release create`/`gh release upload` — it only prints the commands it WOULD
run (plus, unless --offline is given, two READ-ONLY `gh` calls to report
whether the repo/release already exist). Pass --execute to actually publish;
that requires `gh` authenticated with write access to --repo. Publishing the
project's actual pinned release is Task 5.1's job and needs the user's
explicit go-ahead — do not pass --execute from an automated context.

Usage (dry run):
    python publish_mirror.py --staged <staging-dir>/staged.json \\
        --repo nevoflux-browser/engine-assets \\
        --tag engine-b10909-mix-bea84f7

Usage (actually publish, once authorized):
    python publish_mirror.py --staged <staging-dir>/staged.json \\
        --repo nevoflux-browser/engine-assets \\
        --tag engine-b10909-mix-bea84f7 \\
        --execute
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path


def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--staged", required=True, type=Path, help="path to staged.json")
    ap.add_argument("--repo", required=True, help="mirror repo, e.g. nevoflux-browser/engine-assets")
    ap.add_argument("--tag", required=True, help="mirror release tag, e.g. engine-b10909-mix-bea84f7")
    ap.add_argument("--title", default=None, help="release title (default: --tag)")
    ap.add_argument(
        "--offline",
        action="store_true",
        help="skip the read-only gh repo/release existence checks (no gh calls at all in dry-run mode)",
    )
    ap.add_argument(
        "--execute",
        action="store_true",
        help="actually run gh release create/upload (default: dry-run, prints commands only)",
    )
    return ap.parse_args()


def load_staged(path: Path) -> dict:
    with path.open(encoding="utf-8") as f:
        data = json.load(f)
    if not isinstance(data, dict) or not data:
        raise SystemExit(f"{path}: expected a non-empty JSON object keyed by asset name")
    return data


def asset_paths(staged: dict, staged_dir: Path) -> list[Path]:
    paths = []
    missing = []
    for name in staged:
        p = staged_dir / name
        if p.exists():
            paths.append(p)
        else:
            missing.append(name)
    if missing:
        raise SystemExit(
            "staged.json lists assets not present next to it (staging dir moved, or a "
            "download is incomplete?): " + ", ".join(missing)
        )
    return paths


def print_cmd(cmd: list[str], will_run: bool) -> None:
    prefix = "+ " if will_run else "[dry-run, not executed] "
    printable = " ".join(f'"{c}"' if " " in c else c for c in cmd)
    print(prefix + printable)


def gh_read_only(cmd: list[str]) -> tuple[bool, str]:
    """Run a READ-ONLY `gh` call (view, never create/upload). Returns
    (success, stdout-or-stderr). Never raises -- a missing repo/release/`gh`
    binary is reported, not fatal, since this is purely diagnostic."""
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as e:
        return False, str(e)
    if result.returncode == 0:
        return True, result.stdout.strip()
    return False, (result.stderr or result.stdout).strip()


def main() -> None:
    args = parse_args()
    staged = load_staged(args.staged)
    staged_dir = args.staged.parent
    paths = asset_paths(staged, staged_dir)
    title = args.title or args.tag
    notes = f"Engine assets mirror for {args.tag}."

    print(f"staged manifest: {args.staged} ({len(staged)} assets)")
    print(f"mirror repo:     {args.repo}")
    print(f"release tag:     {args.tag}")
    print(f"mode:            {'EXECUTE (will publish)' if args.execute else 'DRY RUN (pass --execute to actually publish)'}")
    print()

    release_exists = False
    if not args.offline:
        repo_ok, repo_info = gh_read_only(["gh", "repo", "view", args.repo, "--json", "name", "-q", ".name"])
        print(f"gh repo view {args.repo}: {'ok (' + repo_info + ')' if repo_ok else 'FAILED: ' + repo_info}")
        rel_ok, rel_info = gh_read_only(
            ["gh", "release", "view", args.tag, "-R", args.repo, "--json", "tagName", "-q", ".tagName"]
        )
        release_exists = rel_ok
        print(
            f"gh release view {args.tag} -R {args.repo}: "
            + (f"exists ({rel_info})" if rel_ok else "does not exist yet (or gh call failed): " + rel_info)
        )
        print()

    asset_args = [str(p) for p in paths]

    if release_exists:
        cmd = ["gh", "release", "upload", args.tag, *asset_args, "-R", args.repo, "--clobber"]
        print(f"release {args.tag} already exists -- would upload/overwrite its {len(paths)} assets:")
        print_cmd(cmd, will_run=args.execute)
        if args.execute:
            subprocess.run(cmd, check=True)
    else:
        cmd = [
            "gh", "release", "create", args.tag, *asset_args,
            "-R", args.repo, "--title", title, "--notes", notes,
        ]
        print(f"release {args.tag} does not exist yet -- would create it with {len(paths)} assets:")
        print_cmd(cmd, will_run=args.execute)
        if args.execute:
            subprocess.run(cmd, check=True)

    print()
    print(f"{len(paths)} assets {'uploaded' if args.execute else 'would be uploaded'}.")
    if not args.execute:
        print("(dry run: no gh release create/upload was actually run -- pass --execute to publish)")


if __name__ == "__main__":
    main()
