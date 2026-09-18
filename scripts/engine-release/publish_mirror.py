#!/usr/bin/env python3
"""Mirror a staged engine release to the nevoflux-browser/engine-assets repo.

Uploads every asset listed in `staged.json` — all 28, including
`llama-prebuilt-manifest.json` (which `gen_release_rs.py` skips for
`EngineRelease`, since no field of it holds fork build metadata — see ruling
R43) — as one GitHub release, so `local::release::mirror_sources`'s
ghfast.top/github.com mirror URLs resolve.

**Dry-run by default, and FULLY OFFLINE by default (controller review round
2, R52).** Without --execute, this script never calls `gh release create`/
`gh release upload`, and by default it makes NO `gh` calls at all — not even
read-only ones — so it is safe to run with no network access and there is
no doubt about what it touched. It just prints the commands it WOULD run
for both possible cases (the release not existing yet, and it already
existing). Pass --check-remote to additionally run two READ-ONLY `gh` calls
(`gh repo view` / `gh release view`) that narrow the dry-run output down to
the one case that actually applies. Pass --execute to actually publish
(this always runs the same read-only existence check first, since it needs
to know whether to create or upload --clobber); that requires `gh`
authenticated with write access to --repo. Publishing the project's actual
pinned release is Task 5.1's job and needs the user's explicit go-ahead —
do not pass --execute from an automated context.

Usage (dry run, fully offline):
    python publish_mirror.py --staged <staging-dir>/staged.json \\
        --repo nevoflux-browser/engine-assets \\
        --tag engine-b10909-mix-bea84f7

Usage (dry run, with read-only gh existence checks):
    python publish_mirror.py --staged <staging-dir>/staged.json \\
        --repo nevoflux-browser/engine-assets \\
        --tag engine-b10909-mix-bea84f7 \\
        --check-remote

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
        "--check-remote",
        action="store_true",
        help="run the read-only gh repo/release existence checks even without --execute "
        "(default dry-run makes no gh calls at all -- R52)",
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
    asset_args = [str(p) for p in paths]

    print(f"staged manifest: {args.staged} ({len(staged)} assets)")
    print(f"mirror repo:     {args.repo}")
    print(f"release tag:     {args.tag}")
    if args.execute:
        mode = "EXECUTE (will publish)"
    elif args.check_remote:
        mode = "DRY RUN, with read-only gh existence checks (pass --execute to actually publish)"
    else:
        mode = "DRY RUN, fully offline -- no gh calls made (pass --check-remote to narrow the output, or --execute to publish)"
    print(f"mode:            {mode}")
    print()

    # Only ever touch the network (even read-only) when actually publishing
    # -- which needs to know whether to create or upload --clobber -- or
    # when the caller explicitly opts in with --check-remote. A dry run
    # must be safe to run with no network access and leave no doubt about
    # what it touched (controller review round 2, R52).
    query_remote = args.execute or args.check_remote

    if query_remote:
        repo_ok, repo_info = gh_read_only(["gh", "repo", "view", args.repo, "--json", "name", "-q", ".name"])
        print(f"gh repo view {args.repo}: {'ok (' + repo_info + ')' if repo_ok else 'FAILED: ' + repo_info}")
        rel_ok, rel_info = gh_read_only(
            ["gh", "release", "view", args.tag, "-R", args.repo, "--json", "tagName", "-q", ".tagName"]
        )
        print(
            f"gh release view {args.tag} -R {args.repo}: "
            + (f"exists ({rel_info})" if rel_ok else "does not exist yet (or gh call failed): " + rel_info)
        )
        print()

        upload_cmd = ["gh", "release", "upload", args.tag, *asset_args, "-R", args.repo, "--clobber"]
        create_cmd = [
            "gh", "release", "create", args.tag, *asset_args,
            "-R", args.repo, "--title", title, "--notes", notes,
        ]
        if rel_ok:
            print(f"release {args.tag} already exists -- would upload/overwrite its {len(paths)} assets:")
            print_cmd(upload_cmd, will_run=args.execute)
            if args.execute:
                subprocess.run(upload_cmd, check=True)
        else:
            print(f"release {args.tag} does not exist yet -- would create it with {len(paths)} assets:")
            print_cmd(create_cmd, will_run=args.execute)
            if args.execute:
                subprocess.run(create_cmd, check=True)
    else:
        # Fully offline: we don't know whether the release exists, so show
        # both possible commands rather than silently guessing one.
        create_cmd = [
            "gh", "release", "create", args.tag, *asset_args,
            "-R", args.repo, "--title", title, "--notes", notes,
        ]
        upload_cmd = ["gh", "release", "upload", args.tag, *asset_args, "-R", args.repo, "--clobber"]
        print(f"If {args.tag} does not exist yet on {args.repo}, this would create it with {len(paths)} assets:")
        print_cmd(create_cmd, will_run=False)
        print(f"If {args.tag} already exists on {args.repo}, this would instead upload/overwrite its {len(paths)} assets:")
        print_cmd(upload_cmd, will_run=False)
        print("(pass --check-remote to find out which of these actually applies)")

    print()
    print(f"{len(paths)} assets {'uploaded' if args.execute else 'would be uploaded'}.")
    if not args.execute:
        print("(dry run: no gh release create/upload was actually run -- pass --execute to publish)")


if __name__ == "__main__":
    main()
