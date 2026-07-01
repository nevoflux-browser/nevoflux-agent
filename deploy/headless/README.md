# NevoFlux Headless — Deploy (P7)

Runs the automation daemon; it spawns a headless browser per task and serves a
task HTTP API. Verified end-to-end (a real `POST /tasks` "navigate example.com,
report title" returned **Example Domain** via a real agent + headless browser).

## Isolation is the container — and granularity matters

The product does **not** build its own sandbox; it assumes it runs **inside** an
isolated environment. **Security depends on the *granularity*, not just presence:**

- **Untrusted workloads → one disposable container per task.** `bash`/`run_command`
  run in the daemon process, so a long-lived container serving many untrusted tasks
  re-opens cross-task poisoning. Use the one-shot `run --task` form, one container
  per task, torn down after.
- **Long-lived service mode** (`--daemon --headless --http-addr`) → **trusted tasks
  only**, or accept the in-container cross-task risk.

The container's **network egress policy** (allowlist to LLM API + task domains) and
**no host credential mounts** (only `/work` + read-only base-profiles) are the hard
boundary. The product adds in-process defense-in-depth (tool allowlist, fs-sandbox,
`SENSITIVE_PATHS`, domain checks) as layers, not the boundary.

## Build

```bash
# stage: dist/nevoflux/ (Linux Gecko build), nevoflux-agent (Linux binary)
docker build -t nevoflux/headless:latest deploy/headless
```

## Run — one task per container (untrusted; recommended)

```bash
docker run --rm \
  --read-only --tmpfs /tmp --tmpfs /var/nevoflux/data \
  --cap-drop ALL --security-opt no-new-privileges \
  --pids-limit 512 --memory 4g --cpus 2 \
  -v "$PWD/out:/work" \
  -v nevoflux-base-profiles:/base-profiles:ro \
  -e LLM_API_KEY="$SHORT_LIVED_KEY" \
  -e HTTP_PROXY="$EGRESS_PROXY" -e HTTPS_PROXY="$EGRESS_PROXY" \
  nevoflux/headless:latest \
  run --task "open example.com and report the title" --profile base1 \
      --policy browser-only --wall-clock 300s --token-budget 200k
# result + debug bundle drained to ./out (result.json, debug-bundle/)
```

## Run — service mode (trusted) + observe

```bash
docker run --rm -p 8080:8080 -p 5900:5900 \
  -e NEVOFLUX_VNC=1 -e NEVOFLUX_VNC_PASSWD=/etc/nevoflux/vncpasswd \
  nevoflux/headless:latest
# submit:  curl -X POST localhost:8080/tasks -d '{"task":"...","mode":"browser"}'
# watch:   curl -N localhost:8080/tasks/<id>/events   (live events)
#          VNC localhost:5900                          (live browser)
# metrics: curl localhost:8080/metrics
```

## k8s (one Job per task)

`restartPolicy: Never`, `activeDeadlineSeconds` = wall-clock, `securityContext`
runAsNonRoot/readOnlyRootFilesystem/seccomp RuntimeDefault/drop ALL, `emptyDir`
for `/work` + data, a `NetworkPolicy` for egress, base-profiles as a RO volume.
Concurrency/warm capacity is the platform's job (the product runs one browser).

## Files
- `Dockerfile` — hardened image (non-root, tini, Xvfb, Gecko libs).
- `entrypoint.sh` — dbus → Xvfb → (optional VNC) → daemon.
- `native-host/com.nevoflux.agent.json` — native-messaging manifest (verify path via `about:support`).
