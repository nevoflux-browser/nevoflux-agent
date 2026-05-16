# External-mode eval — manual playbook

External mode (`--browser-mode external`) runs eval against a locally-running
nevoflux instance. The dev nevoflux is responsible for launching the daemon
via Native Messaging; eval reads the dev-instance lock to discover the HTTP
bridge.

## One-time setup

Build the daemon binary with the `eval-mock-llm` feature (so CI-deterministic
runs work; real-LLM runs ignore the mock and use the daemon's configured
provider):

```bash
cd /ai/project/nevoflux-agent
cargo build --release --bin nevoflux-agent --features eval-mock-llm
```

## Launch the dev nevoflux instance

In the `nevoflux` repo (the browser overlay), start the dev build with the
eval+dev-instance env vars set:

```bash
cd /ai/project/nevoflux
NEVOFLUX_EVAL_MODE=1 \
NEVOFLUX_EVAL_RUN_ID=dev-$(date +%Y%m%d-%H%M%S) \
NEVOFLUX_DEV_INSTANCE_MODE=1 \
NEVOFLUX_EVAL_LLM_MODE=mock \
npm run start
```

(Omit `NEVOFLUX_EVAL_LLM_MODE=mock` for real-LLM runs with the daemon's
configured provider/API key.)

When the browser opens, the extension launches the daemon via Native
Messaging. The daemon detects the env vars and:
- Writes a dev-instance lock at `$XDG_STATE_HOME/nevoflux-dev/daemon.lock`
- Starts the eval bridge HTTP listener
- (Optional) Spawns the mock LLM server

## Run eval against it

In another shell, in `nevoflux-agent`:

```bash
just eval-dev online-mind2web 3
```

Expected:
- Eval reads the dev-instance lock
- Submits each task with browser tools enabled
- Agent navigates to the task URL via the real browser
- WebJudge scores each final_answer
- Report appears in `eval/reports/exploratory/`

## Troubleshooting

- `dev-instance lock file not found` → the dev nevoflux was launched without
  `NEVOFLUX_DEV_INSTANCE_MODE=1` and `NEVOFLUX_EVAL_MODE=1`. Restart with
  both env vars set.
- `dev instance pid X no longer alive` → the dev browser exited but the lock
  file is stale. Delete `$XDG_STATE_HOME/nevoflux-dev/daemon.lock` and
  relaunch.
- WebJudge times out → the daemon's mock LLM endpoint isn't reachable from
  eval. Inspect daemon.log for the mock server's bound address; set
  `NEVOFLUX_WEBJUDGE_BASE_URL=http://127.0.0.1:<port>` to match.
