# NevoFlux headless automation image (P7).
# Runs the automation daemon, which spawns a headless browser per task.
# Isolation is the CONTAINER (per v2 §0 — the product does not build its own
# sandbox); for untrusted workloads run ONE disposable container per task.
FROM debian:bookworm-slim

# --- system deps: Xvfb (Linux headless display) / x11vnc / dbus / Gecko libs ---
RUN apt-get update && apt-get install -y --no-install-recommends \
      xvfb x11vnc dbus-x11 xauth x11-utils tini ca-certificates git curl unzip \
      libgtk-3-0 libdbus-glib-1-2 libxt6 libasound2 libx11-xcb1 libxtst6 libpci3 \
      fonts-liberation fonts-noto-color-emoji fonts-noto-cjk \
    && rm -rf /var/lib/apt/lists/*

# --- non-root user + dirs ---
RUN useradd -m -u 10001 nevo \
 && mkdir -p /var/nevoflux/data /work /base-profiles /opt/packs \
 && chown -R nevo:nevo /var/nevoflux /work /base-profiles /opt/packs

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

# --- (optional) bundle packs at build time -------------------------------------
# No daemon needed at build: just CLONE the pack repos into the image. The
# entrypoint installs them once the daemon is up at startup (see entrypoint.sh),
# so it works even with an ephemeral (tmpfs) data dir — packs are re-applied each
# start from these baked-in clones.
#   Build with:  docker build --build-arg PACK_REPOS="owner/repo https://github.com/x/y.git" ...
#   (space-separated: github "owner/repo" shorthand, or a full git URL)
ARG PACK_REPOS=""
RUN if [ -n "$PACK_REPOS" ]; then set -eu; \
      for repo in $PACK_REPOS; do \
        case "$repo" in *://*|*@*) url="$repo";; *) url="https://github.com/$repo.git";; esac; \
        name="$(basename "${repo%.git}")"; \
        echo "cloning pack repo: $url -> /opt/packs/$name"; \
        git clone --depth 1 "$url" "/opt/packs/$name"; \
      done; \
    fi

# --- GBrain (knowledge/memory brain): installed + enabled by default -----------
# gbrain is a TypeScript tool run via `bun`; the daemon spawns it at startup and
# auto-resolves it from ~/.bun + ~/.nevoflux/brain-tool. Installing it here (no
# daemon needed at build) makes brain tools + brain.* RPCs available out of the box.
# Its LLM calls route through the in-process gateway to the active [llm].provider,
# so no separate gbrain key is needed — just the provider's *_API_KEY at runtime.
#   Disable with --build-arg INSTALL_GBRAIN=0 ; pin with --build-arg GBRAIN_PIN=...
ARG INSTALL_GBRAIN=1
ARG GBRAIN_PIN="github:garrytan/gbrain#090bb53"
ARG LLM_PROVIDER="anthropic"
RUN if [ "$INSTALL_GBRAIN" = "1" ]; then set -eu; \
      curl -fsSL https://bun.sh/install | bash; \
      BUN="$HOME/.bun/bin/bun"; \
      mkdir -p "$HOME/.nevoflux/brain-tool" "$HOME/.gbrain" "$HOME/.config/nevoflux"; \
      ( cd "$HOME/.nevoflux/brain-tool" && "$BUN" add "$GBRAIN_PIN" ); \
      printf '[llm]\nprovider = "%s"\n\n[knowledge_base]\nenabled = true\n\n[knowledge_base.brain]\nenabled = true\n' \
        "$LLM_PROVIDER" > "$HOME/.config/nevoflux/config.toml"; \
      echo "gbrain installed ($GBRAIN_PIN); brain enabled; [llm].provider=$LLM_PROVIDER (key from *_API_KEY env at runtime)"; \
    fi

VOLUME ["/work"]

ENTRYPOINT ["/usr/bin/tini","--","/usr/local/bin/entrypoint.sh"]
# Long-running service (serves the task API); for untrusted use `run --task` one-shot.
CMD ["--daemon","--headless","--http-addr","0.0.0.0:8080"]
