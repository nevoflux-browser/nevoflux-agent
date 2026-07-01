# NevoFlux headless automation image (P7).
# Runs the automation daemon, which spawns a headless browser per task.
# Isolation is the CONTAINER (per v2 §0 — the product does not build its own
# sandbox); for untrusted workloads run ONE disposable container per task.
FROM debian:bookworm-slim

# --- system deps: Xvfb (Linux headless display) / x11vnc / dbus / Gecko libs ---
RUN apt-get update && apt-get install -y --no-install-recommends \
      xvfb x11vnc dbus-x11 xauth x11-utils tini ca-certificates \
      libgtk-3-0 libdbus-glib-1-2 libxt6 libasound2 libx11-xcb1 libxtst6 libpci3 \
      fonts-liberation fonts-noto-color-emoji fonts-noto-cjk \
    && rm -rf /var/lib/apt/lists/*

# --- non-root user + dirs ---
RUN useradd -m -u 10001 nevo \
 && mkdir -p /var/nevoflux/data /work /base-profiles \
 && chown -R nevo:nevo /var/nevoflux /work /base-profiles

# --- nevoflux (Linux Gecko build) + agent binary + entrypoint ---
COPY --chown=nevo:nevo dist/nevoflux/            /opt/nevoflux/
COPY --chown=nevo:nevo nevoflux-agent            /usr/local/bin/nevoflux-agent
COPY --chmod=0755 entrypoint.sh                  /usr/local/bin/entrypoint.sh

# --- native-messaging host manifest (Firefox launches the proxy from here) ---
# NOTE: verify the search path for this fork via about:support → Native Messaging.
COPY --chown=nevo:nevo native-host/com.nevoflux.agent.json \
     /home/nevo/.mozilla/native-messaging-hosts/com.nevoflux.agent.json

# --- env: pin data dir (daemon + proxy agree on daemon-managed.port) + display ---
# The daemon's build_headless_runner reads these; browser_launch propagates
# NEVOFLUX_PROXY_ROLE/NEVOFLUX_DATA_DIR to the spawned browser (verified E2E).
ENV NEVOFLUX_DATA_DIR=/var/nevoflux/data \
    DISPLAY=:99 \
    HOME=/home/nevo \
    NEVOFLUX_BROWSER_BIN=/opt/nevoflux/nevoflux \
    NEVOFLUX_BASE_PROFILES=/base-profiles \
    MOZ_DISABLE_CONTENT_SANDBOX=1

USER nevo
WORKDIR /work
VOLUME ["/work"]

ENTRYPOINT ["/usr/bin/tini","--","/usr/local/bin/entrypoint.sh"]
# Long-running service (serves the task API); for untrusted use `run --task` one-shot.
CMD ["--daemon","--headless","--http-addr","0.0.0.0:8080"]
