# Eval harness (command-line, no Docker)

Runs the J20 browser-execution regression set and the Jev A/B set against a
real headless NevoFlux, one trial at a time, on a Windows dev box.

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

- `/v1/chat/completions` still reports zero usage on the agent-loop route
  (`FinishPayload` carries no usage); the harness uses `/tasks`, which does.
