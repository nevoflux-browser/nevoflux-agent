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

## Known gaps

- Agent-loop tasks have no deadline on the daemon side: `wall_clock_secs`
  bounds only the script backend, and `DELETE /tasks/:id` only marks the task
  cancelled in the queue while the agent keeps running. The harness's poll
  timeout (task timeout + follow-up delays + 60s) plus killing the daemon is
  the real bound.

- `/v1/chat/completions` still reports zero usage on the agent-loop route
  (`FinishPayload` carries no usage); the harness uses `/tasks`, which does.
