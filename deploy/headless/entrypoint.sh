#!/usr/bin/env bash
# NevoFlux headless entrypoint (P7): dbus → Xvfb → (optional VNC) → daemon.
# On Linux the browser needs a display; Xvfb provides a virtual one. (On
# Windows the daemon uses native -headless and needs no Xvfb — see the E2E.)
set -euo pipefail

# 1) dbus session (Firefox needs it)
eval "$(dbus-launch --sh-syntax)"

# 2) Xvfb virtual display
Xvfb "${DISPLAY}" -screen 0 1280x1024x24 -nolisten tcp &
for _ in $(seq 1 100); do xdpyinfo -display "${DISPLAY}" >/dev/null 2>&1 && break; sleep 0.1; done

# 3) optional live VNC (default off; NEVOFLUX_VNC=1 + a password file to enable)
if [ "${NEVOFLUX_VNC:-0}" = "1" ]; then
  x11vnc -display "${DISPLAY}" -rfbauth "${NEVOFLUX_VNC_PASSWD:-/etc/nevoflux/vncpasswd}" \
         -forever -shared -bg -rfbport 5900
fi

# Install packs cloned into the image at build time (Dockerfile ARG PACK_REPOS).
# Pack install goes over the daemon's RPC, so this can only run once a daemon is
# up — hence startup, not build. Idempotent each start (works with a tmpfs data
# dir; --force re-applies on a persistent one).
install_bundled_packs() {
  [ -d /opt/packs ] && [ -n "$(ls -A /opt/packs 2>/dev/null)" ] || return 0
  for _ in $(seq 1 60); do [ -f "${NEVOFLUX_DATA_DIR}/daemon.port" ] && break; sleep 1; done
  for p in /opt/packs/*/; do
    [ -d "$p" ] || continue
    src="$p"; [ -f "${p}pack.toml" ] && src="${p}pack.toml"
    echo "installing bundled pack: $src"
    nevoflux-agent pack install "$src" --yes --force || echo "WARN: pack install failed: $src"
  done
}

case " $* " in
  *" --daemon "*)
    # Daemon mode: start in background so we can install bundled packs once it's
    # up, then hand over. Forward SIGTERM/SIGINT for a clean shutdown.
    nevoflux-agent "$@" &
    DAEMON_PID=$!
    trap 'kill -TERM "$DAEMON_PID" 2>/dev/null || true' TERM INT
    install_bundled_packs
    wait "$DAEMON_PID"
    ;;
  *)
    # Non-daemon (e.g. one-shot `run --task`, `pack`, `config`): run directly.
    exec nevoflux-agent "$@"
    ;;
esac
