# NevoFlux Eval

Evaluation harness for NevoFlux. Three modes for three purposes: dev iteration, daemon-side regression, and authoritative release scoring.

## TL;DR — which mode do I want?

| Goal | Mode | Signal Grade | Cost |
|---|---|---|---|
| "Did my agent change improve memory recall?" | `daemon-only` | Exploratory | $0 |
| "Did my Canvas JS change break the SDK?" | `external` (dev) | Exploratory | low |
| "What's the official score for our v0.3.2 release?" | `release` | Authoritative | ~$30-50 |

**Authoritative reports go to `eval/reports/authoritative/` (committed to git, safe to publish).**
**Exploratory reports go to `eval/reports/exploratory/` (gitignored, never publish).**

## Quick start

```bash
# Pull benchmark submodules
git submodule update --init --recursive

# Set API keys
cp eval/.env.example eval/.env
# Edit eval/.env

# 1. Daemon-only run (fastest, no browser)
just eval-daemon nevoflux-suite

# 2. Dev-mode run (start nevoflux locally first)
#    In nevoflux repo:
#      npm run build        # once after JS changes
#      just dev             # starts on :5959
#    In nevoflux-agent repo:
just eval-dev online-mind2web 20

# 3. Authoritative run (downloads published release binary)
just eval-release v0.3.2 online-mind2web
```

## Three browser modes

### `--browser-mode daemon-only`
- No browser launched.
- Tasks with `requires_browser: true` are **skipped** (excluded from accuracy denominator).
- Signal grade: **Exploratory**.
- Use: daily nightly CI, agent-only iteration, quick smoke tests.

### `--browser-mode external --browser-endpoint <url>`
- Connects to an already-running nevoflux instance.
- You started it manually (typically `just dev` in the nevoflux repo).
- Signal grade: **Exploratory** (dev build, not a published artifact).
- Use: iterating on Canvas runtime, browser overlay, or any cross-stack change.

### `--browser-mode release --browser-version <tag>`
- Downloads a published nevoflux release tarball from GitHub.
- Caches by version under `.cache/nevoflux-releases/<version>/`.
- Launches headlessly with `--remote-debugging-port=5959`.
- Signal grade: **Authoritative** — these are the only numbers safe to publish.
- Use: release verification, CI-driven score updates, leaderboard submissions.

## What's in here

| Directory | Purpose |
|---|---|
| `crates/eval/` | Eval harness (Rust) — runner, browser launcher, judges, metrics, reporter |
| `eval/benchmarks/` | Third-party benchmark data (git submodules) |
| `eval/nevoflux-suite/` | Self-suite: YAML tasks for capabilities unique to NevoFlux |
| `eval/reports/authoritative/` | Authoritative reports (release-mode runs) — committed |
| `eval/reports/exploratory/` | Exploratory reports (daemon-only / external runs) — gitignored |
| `eval/reports/trends.json` | Time-series feed (authoritative only) for dashboards |

## Adding a self-suite task

```yaml
# eval/nevoflux-suite/<category>/<task>.yaml
id: my-task-001
category: memory_recall      # or canvas_sdk | mcp_bidir | mode_authz | privacy_audit
mode: agent
requires_browser: false      # set to true ONLY if real browser is needed
prompt: "What you ask the agent"
setup: []
assertions:
  - type: contains_any
    targets: ["expected substring"]
```

Open a PR. CI runs the daemon-only tier automatically. Tasks with `requires_browser: true` run only in nightly full-stack or on release.

## Benchmark coverage

| Benchmark | Tier | Browser needed | Default Judge |
|---|---|---|---|
| `nevoflux-suite` | P0 | varies per task | structured |
| `browsecomp` | P0 | no (short answers) | programmatic |
| `browsecomp-zh` | P0 | no (short answers) | programmatic |
| `online-mind2web` | P0 | yes | webjudge (o4-mini) |
| `webvoyager` | P1 | yes | webjudge (GPT-4V) |
| `webarena` | P1 | no (Docker sandbox) | programmatic |
| `osworld` | P1 | partial | programmatic |
| `mind2web-2` | P2 | yes | agent-as-judge |

## CI strategy

| Workflow | Trigger | Browser mode | Cost / run |
|---|---|---|---|
| `eval-pr.yml` | every PR | daemon-only | $0 |
| `eval-nightly.yml` | daily 08:00 UTC | daemon-only (BrowseComp ok in daemon mode) | ~$10 |
| `eval-fullstack.yml` | nevoflux release / manual / tag | **release** | ~$30-50 |

**No self-hosted runners. All workflows run on GitHub-hosted.** Full-stack eval downloads the nevoflux release binary as a build artifact.

## Reports

### Authoritative
- File pattern: `eval/reports/authoritative/<benchmark>-<browser_version>.md`
- Committed to git
- Each report header includes `Browser: nevoflux-vX.Y.Z` and `Signal grade: Authoritative`
- Used for: HN/X publication, leaderboard submissions, release notes

### Exploratory
- File pattern: `eval/reports/exploratory/<timestamp>-<benchmark>-<browser>.md`
- Gitignored (see `eval/reports/.gitignore`)
- Header includes a 🔬 banner reminding readers NOT to publish
- Used for: dev iteration only

### Trends
- `eval/reports/trends.json` — JSONL, **authoritative runs only**
- Exploratory runs are deliberately skipped to avoid polluting time-series
- Feeds the public dashboard (TODO: `eval.nevoflux.dev`)

## Reproducing a published score

Every authoritative report tells you exactly how to reproduce it:

```bash
# Header from eval/reports/authoritative/online-mind2web-nevoflux-v0.3.2.md says:
# - Browser: nevoflux-v0.3.2
# - LLM: Claude Sonnet 4.6
# - Eval framework: nevoflux-eval 0.1.0

git checkout eval-v0.1.0
just eval-release v0.3.2 online-mind2web
# → produces a fresh report with the same setup
```

## Threat model — what eval does NOT do

- Exploratory reports never enter the trends file or git history
- API keys live only in `eval/.env` (gitignored) or GitHub Actions Secrets
- Sensitive-info scan (detect-secrets) gates every CI commit
- No traces or screenshots uploaded as CI artifacts

## Contributing

See `CONTRIBUTING-EVAL.md` (TODO) for three contribution paths:
1. **Add a task** — YAML only, no Rust required
2. **Add a benchmark adapter** — implement `Benchmark` trait
3. **Add a judge / metric** — implement `Judge` or `Metric` trait

## License

Eval code: AGPL-3.0 (matches main repo). Third-party benchmark data: see each submodule's LICENSE.
