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

# 4) automation daemon (spawns the browser per task, serves the task API)
exec nevoflux-agent "$@"
