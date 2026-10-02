# Eval harness (command-line, no Docker)

Runs the J20 browser-execution regression set and the Jev A/B set against a
real headless NevoFlux, one trial at a time, on a Windows dev box.

## Usage

Build the daemon first (`cargo build -j 3 --release`); the runner uses
`target/release/nevoflux-agent.exe` and the dev browser under
`nevoflux/engine/.../dist/bin/nevoflux.exe` (`--agent-exe`, `--browser-bin`
to override). From the repo root, in Git Bash:

```bash
# J20 regression set, 3 passes, on the Kimi (k3) Anthropic-wire provider
PYTHONIOENCODING=utf-8 python -m eval.harness.run \
  --tasks eval/harness/tasks/j20 --k 3 --set llm.provider=anthropic \
  --out eval/results/baseline-j20

# delta and cost from a finished run
python -m eval.harness.calibrate eval/results/baseline-j20/trials.jsonl
```

- `--set section.key=value` edits the trial's copy of your config (never your
  own file); repeatable. Values parse as bool/int/float/string. The trial
  fails if the daemon did not load the requested `llm.provider`, or fell back
  to default config.
- `--only id1,id2` runs a subset; `--keep-dirs` keeps each trial directory
  (config, `daemon.stdout.log`, `data/`) for debugging.
- **Resume**: re-run the same command with the same `--out`. Trials that
  already have a real result are skipped; `provider_error` / `harness_error`
  / `timeout` trials run again. Only the latest row per (pass, task) counts.
- **Exit code 2**: three provider errors in a row (provider down, or a
  subscription quota such as Kimi's 5-hour window). Wait and re-run.
- Output: `<out>/trials.jsonl` (one row per trial: status, pass, per-check
  reasons, turn outputs, per-turn usage, provider/model actually loaded, site
  events, exported session JSONL), `<out>/summary.json`, `<out>/run.json`.
- Statuses: `succeeded`/`failed` are agent results; `provider_error` (the
  daemon reports provider failures as a succeeded task whose output is the
  error text), `timeout`, `harness_error` are missing trials: they never
  pass, they count 0 in the score, and more than 10% missing sets
  `eval_invalid` in calibrate.

Provider notes: `custom:chinamobile` (deepseekv4-flash) rejects any image, so
it fails every trial where the agent takes a screenshot; use a vision model.
A J20 trial reads about 230k input tokens (Jev about 350k) before P0's prompt
caching; Kimi's coding plan runs out after roughly 25 trials per 5 hours.

Tasks are JSON files under `tasks/<set>/`; `{base}` is the task's site root
and `{root}` the server root. Pick-one and count tasks ask for a final
`ANSWER:` / `答案：` line and check only that line.

## Environment prerequisites (verified 2026-09-26)

Each trial runs its own daemon:

```bash
NEVOFLUX_DATA_DIR=<trial>/data            # db, port files, proxy.log
NEVOFLUX_CONFIG=<trial>/config.toml       # copy of the user's config, with overrides
NEVOFLUX_BROWSER_BIN=C:/Users/Docker/nevoflux/nevoflux/engine/obj-x86_64-pc-windows-msvc/dist/bin/nevoflux.exe
NEVOFLUX_BASE_PROFILES=<trial>/base-profiles   # empty dir is fine: `default` clones to an empty profile
NEVOFLUX_PROFILE_WORK=<trial>/profiles
target/release/nevoflux-agent.exe --daemon --headless --http-addr 127.0.0.1:<http> --port <internal>
```

Then `POST /tasks` and poll `GET /tasks/<id>`.

- There is **no** `run --task` subcommand (the deploy README mentions one);
  headless mode is `--daemon --headless --http-addr` plus the task API.
- The browser launches without `-headless`, so **windows do appear** while a
  trial runs. Leave them alone.
- The browser's extension starts the native-messaging proxy registered in
  `%APPDATA%\Mozilla\NativeMessagingHosts\com.nevoflux.agent.json`. On this
  box that points at the **installed release** agent
  (`C:\Program Files\NevoFlux Browser\distribution\bin\nevoflux-agent.exe`).
  It worked against the dev daemon in the smoke run. If a trial ever stalls
  with `browser did not register`, suspect a protocol mismatch first: copy
  the dev `target\release\nevoflux-agent.exe` into
  `engine\obj-x86_64-pc-windows-msvc\dist\bin\distribution\bin\` and retry.
- The proxy finds the daemon through `daemon.port` in `NEVOFLUX_DATA_DIR`.
  Without `--port`, the daemon binds the fixed port **19500**, the same one
  the desktop NevoFlux uses. The harness always passes a free `--port`. If
  you start a daemon by hand, close NevoFlux first.
- Isolation gap: `NEVOFLUX_CONFIG` moves `config.toml` only. Other files the
  daemon reads from `%APPDATA%\nevoflux\` (e.g. `space_souls.toml`) are
  still the user's.
- Daemon logs: `<trial>/daemon.stdout.log` (stdout+stderr) and
  `<trial>/data/proxy.log`. Nothing goes to the runner's terminal.
- A dead trial leaves processes behind? Kill everything whose command line
  mentions the trial directory:
  `Get-CimInstance Win32_Process | ? { $_.CommandLine -like "*<trial dir>*" } | % { Stop-Process -Id $_.ProcessId -Force }`

## Smoke results

- 2026-09-26: `Open http://127.0.0.1:18080/hello.html and tell me the exact page title.`
  → `succeeded`, output `The page title is "NF Smoke 42".`
- With `[llm] provider` overridden to a nonexistent name in the trial config,
  the same task failed with `No API key configured`. So the daemon reads
  `NEVOFLUX_CONFIG`, not the user's config.

## Baseline (2026-09-27)

Provider `llm.anthropic` = Kimi `k3` (Anthropic wire, `https://api.kimi.com/coding/`),
chosen by the user; agent built from this branch (screenshot fixes included),
browser = dev build `nevoflux.exe` (engine obj, 2026-09-09), no prompt caching
yet (pre-P0). Runs took ~16 h wall clock because of Kimi's 5-hour quota.

**J20, k=3** (`eval/results/baseline-j20`): per-pass 0.90 / 1.00 / 0.95,
mean **0.95**, sd 0.05, **δ = 0.10**, 0 missing trials. About 179k input
tokens and 92 s per trial; 10.75M input tokens in total.

- Flaky tasks, both real agent failures, and both "claimed success it did not
  achieve":
  - `j20-flights-search` (1/3): never ticked "Direct flights only"; filtered
    the results itself instead.
  - `j20-select-form` (2/3): reported gift wrap as ticked; the submitted form
    had `gift: false`.
- δ = 0.10 misses the v1.4 G1 target (δ ≤ 0.03). This isn't noise to be
  engineered away: with 20 tasks, one task flipping moves the score by 0.05,
  so two flaky tasks at k=3 already give sd 0.05. Getting δ ≤ 0.03 needs
  either many more tasks or a much larger k. That decision belongs to G1,
  not the harness.

**Jev set, k=1 dry run** (`eval/results/jev-dry-k3-v2`): 29/30, 0 missing.
About 131k input tokens and 134 s per trial. The one failure,
`jev-zh-shop-change`, exposed a site gap: the cart had no remove control
(fixed afterwards), and the agent then claimed to have emptied the cart. The
Jev set's δ calibration is deferred until after P0 on purpose. G2 compares
Jev on/off against the *post-P0* baseline, so a pre-P0 Jev baseline wouldn't
be the reference.

Note: the J20 baseline ran before the cart page gained its remove button;
only `j20-shop-cart` opens the cart, and it doesn't use removal.

## P0 (prompt caching), 2026-09-29

Same provider, browser and task set as the baseline; agent from
`feat/p0-prompt-caching` (all Anthropic traffic on the raw Messages path with
cache breakpoints, stable prompt prefix, watermark shrink).

**J20, k=1** (`eval/results/p0-j20`): **20/20 (1.00)**, within δ of the
0.95 baseline; 0 missing trials; 81 s per trial (baseline 92 s).

| per trial | baseline (rig) | P0 |
|---|---|---|
| calls | 12.1 | 11.6 |
| input reported | 179k | 267k |
| of which cache reads | not reported | 262k |
| **uncached input** | **179k** | **4.9k** |

- Cache hit ratio **98.2%** over the run; every trial ≥ 95%.
- The baseline's "input" is only the uncached part: rig's streaming usage
  drops Kimi's `cache_read_input_tokens` and its `input_tokens` excludes
  them. The true prompt size is the P0 column (about 23k per call, which
  matches the daemon's own estimate of the first call, 22.9k). So the
  comparison that matters is uncached input: 179k → 4.9k per trial.
- No double counting: Kimi reports `input_tokens: 0` plus
  `cache_read_input_tokens` on `message_delta`; the reported prompt equals
  the estimate.
- Kimi reports `cache_creation_input_tokens: 0` always, so `cache_write` is
  0 here; that says nothing about other endpoints.
- No J20 task loads a skill, so the skill-turn cache miss (system prompt is
  one block) is not measured by this run.
- The first attempt failed every trial with an empty reply: Kimi writes SSE
  lines as `data:{…}` with no space, and the raw parser required `data: `.
  Fixed before this run.

## J20-A (single-fire clicks), 2026-09-29

Browser `feat/j20a-single-fire`: each click is sent once (a lower tier only
if the one above sent nothing), a covered target is refused instead of
clicked through, checkbox/select/aria state counts as a click effect,
`click_by_id` stops at the first click it sends, the content-script path
never re-sends a mutation, and password/file values stay out of snapshots.
Agent main (P0). J20 now has 21 tasks (`j20-checkbox-once` added: tick a
checkbox, save; the box must change exactly once).

**J20, k=1** (`eval/results/j20a`): **21/21 (1.00)**, 0 missing after a
resume; 76 s per trial (P0: 81 s); cache hit 97.9%.

- `j20-checkbox-once`: 1 toggle, saved with `updates: true`.
  `j20-select-form` (the baseline's "gift wrap ticked, then unticked"
  failure) passes.
- `j20-slow-single-pay` / `j20-slow-report-effect`: `pay_click` = 1.
- `j20-overlay-no-blind-click`: 0 clicks on the overlay. The model saw the
  cookie banner in the snapshot and accepted it first, so the new
  "covered" refusal never fired in this run; the snapshot's own occlusion
  filter keeps covered targets out of view. The refusal is unit-tested only.
- `j20-shadow-input`, `j20-iframe`, `j20-scroll-find` pass (shadow hit-testing,
  scroll-into-view).
- One trial (`j20-radio-not-text`) first timed out with **zero** session
  events — the agent never ran a step — and passed on the resume (99 s).
  Not attributable to the click changes; cause not found (the trial dir was
  not kept).

## J20-B (real roles, node-bound ids, select options), 2026-09-30

Browser `feat/j20b-identity`: a11y roles read from Gecko (the old
role-number table matched no Gecko, so the a11y half of every snapshot was
dead), a retry when a fresh page's accessible tree is not built yet, ids
from an actor-side registry (a node keeps its id; nothing written into the
page), `actOnRef` acting on exactly that node after a staleness check,
every `<select>` option listed as a target, a waits-for-options step after
opening a popup, and a capped `## text` section of visible text. Agent
`feat/j20b-exposure`: `browser_click/type/fill` (CSS selector) moved
behind `load_selector_tools`.

**J20, k=1** (`eval/results/j20b2`): **21/21**, 0 missing, 64 s per trial,
cache hit 98.0%.

| per run (21 trials) | J20-A | J20-B, first run | J20-B |
|---|---|---|---|
| `browser_eval_js` calls | 120 | 96 | **60** |
| snapshot lines `?tag` | 1464 | 1608 | **165** |
| snapshot lines with a role | 58 | 392 | **951** |
| select option lines | 0 | 36 | 32 |
| stale-id refusals | – | 5 (all false) | 0 |
| `load_selector_tools` calls | – | 4 | 4 |
| s per trial | 75.6 | 67.4 | 64.1 |

- The first J20-B run (`eval/results/j20b`, also 21/21) exposed two
  defects, fixed before the second: the staleness fingerprint read the
  a11y role, which Gecko creates lazily, so five actions were refused as
  "now a combobox/textbox" when nothing had changed (the kind now comes
  from the DOM); and the first snapshot of every page was all `?tag`
  because the accessible tree was not built yet (one retry after ≤150 ms).
- One first-run trial (`j20-select-form`) timed out with zero session
  events — the same startup hang as in the J20-A run — and passed on resume.

**After the final review's fixes** (`eval/results/j20b3`; selector tools
offered in the same run once loaded, shadow-DOM controls kept by the
occlusion filter, one a11y walk per snapshot, a viewport-pruned text walk,
✓/☐ on toggles, the element's label in the staleness fingerprint):
**21/21**, 43 s per trial, cache hit 96.7%, `browser_eval_js` 48 (J20-A
120), `?tag` lines 58 (J20-A 1464), 0 stale refusals, 56 ☐ marks.
`j20-shadow-input` is now done by id alone (2 × `fill_by_id`, 2 ×
`click_by_id`); `j20-iframe` still goes through `eval_js` — iframe controls
get no ids yet (they need frame-offset clicking). The run was resumed twice
across Kimi's 5-hour quota window; the harness now counts the raw path's
"Internal error: Anthropic-raw …" as a provider error.

## J20-C (iframe ids, ids for the selector-only tools), 2026-10-01

Browser `feat/j20c-frames`: same-origin iframe controls are listed (rects
in top coordinates, occlusion tested in their own frame) and acted on in
their own document/window; `actOnRef` also runs probe/paste/rich fill/text/
wait/upload; background routes the reserved `ref:eN` selector to it; fill
accepts `text` (fixes `browser_input` on plain inputs). Agent
`feat/j20c-ids`: `element_id` on `browser_input`/`browser_probe`/
`browser_upload_file`/`browser_wait_for`; upload routed in the native loop
(it fell through to MCP before); the element cache shows only CSS
selectors. Plus the J20-B minors.

**J20, k=1** (`eval/results/j20c`): **21/21**, 0 missing, 53 s per trial
(run-to-run variance; J20-B final 43 s), cache hit 96.7%,
`browser_eval_js` 36 (J20-B final 48, J20-A 120). `j20-iframe` is now
solved by id (2 × `browser_navigate`, 2 × `browser_click_by_id`; J20-B:
10 × `browser_eval_js` + 4 screenshots).

**After the review fixes** (`eval/results/j20c2`): 20/21. `j20-zh-messenger`
picked the decoy "Zhang Sanfeng", and `browser_eval_js` rose to 102. The
display had switched to 125% scaling (viewport 955 → 717 tall), and the a11y
walk was subtracting CSS-pixel `mozInnerScreenX/Y` from device-pixel
`getBounds()`. The occlusion sampling therefore landed too low and dropped
the top of lists as "occluded" (`stats.occluded: 2`; the real contact was not
in the snapshot). Fixed by `a11yBoundsToViewport`.

**Final** (`eval/results/j20c3`, 125% scaling): **21/21**, 49 s per trial,
cache hit 98.0%, `browser_eval_js` 26 (same count method as above).
Rule: if a rerun regresses with no code cause, compare the snapshot's
`viewport:` line and `stats.occluded` against the passing run first.

## Jev runs: `--site-host localtest.me`

The daemon treats 127.0.0.1 as intranet (§5.8), so pages from the eval sites
are never sent to Jev and every grade falls back to the local rule. Jev runs
therefore serve the sites under `localtest.me`, a public DNS name that
resolves to 127.0.0.1: `--site-host localtest.me`. The harness checks before
any trial that the name resolves to loopback only, and stops otherwise. The
default stays 127.0.0.1, which needs no DNS.

## Jev P2-2 (visibility), 2026-10-02

Agent `feat/jev-p2-2`: tool results over 4 KB in the native loop are graded
by Jev (hide/short/long/full, block Nouls at 0.3) and rendered before they
enter the context; the full text is spilled and `recall(chunk_id)` brings it
back; aged-shrink is off while visibility is on. Sensitive or unknown pages
are never sent (local rule, `graded_by: sensitive`); Jev failures fall back to
short + recall (`graded_by: fallback`).

**J20, k=1, Jev off** (`eval/results/p2-2-off`): **21/21**, 51 s per trial,
cache hit 97.3%, main input 3.00M, `browser_eval_js` 36 — unchanged.

**J20, k=1, Jev on** (`eval/results/p2-2-on`): **21/21**, 53 s per trial,
cache hit 97.2%, main input 3.30M (+10%), `browser_eval_js` 49. **Jev was
never asked**: one result went over 4 KB and it was graded `sensitive`, because
every eval site is served from 127.0.0.1, which the privacy rule treats as
intranet (§5.8). The model used `recall` once on it. The +10% input is the
cost of aged-shrink being off with nothing graded in its place.

Consequence: neither J20 nor the `jev` task set can measure Jev grading (or
G2) while the sites are on loopback; that needs a decision (a public host for
the eval sites, or an eval-only exception to the loopback rule).


- Agent-loop tasks have no deadline on the daemon side: `wall_clock_secs`
  bounds only the script backend, and `DELETE /tasks/:id` only marks the task
  cancelled in the queue while the agent keeps running. The harness's poll
  timeout (task timeout + follow-up delays + 60s) plus killing the daemon is
  the real bound.

- `/v1/chat/completions` still reports zero usage on the agent-loop route
  (`FinishPayload` carries no usage); the harness uses `/tasks`, which does.
