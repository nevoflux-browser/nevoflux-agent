#!/bin/bash
# Native Messaging Host Setup for NevoFlux Agent
# Supports Firefox/NevoFlux on Linux/macOS/Windows(MSYS)
#
# Binary resolution order:
#   1. Command line argument: ./setup.sh /path/to/nevoflux-agent
#   2. Environment variable: NEVOFLUX_AGENT_BIN=/path/to/binary
#   3. PATH lookup: which nevoflux-agent
#   4. Local development build: ../../target/release/nevoflux-agent
#   5. GitHub release download (latest from dorisgyl/nevoflux-agent)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Configuration
AGENT_BIN_NAME="nevoflux-agent"
EXTENSION_ID="agent@nevoflux.com"
MANIFEST_NAME="com.nevoflux.agent"
GITHUB_REPO="dorisgyl/nevoflux-agent"

echo "=== NevoFlux Native Messaging Host Setup ==="
echo ""

# Detect platform and architecture
detect_platform() {
  case "$(uname -s)" in
    Linux*)  echo "linux" ;;
    Darwin*) echo "macos" ;;
    MINGW* | MSYS* | CYGWIN*) echo "windows" ;;
    *) echo "unknown" ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64 | amd64)  echo "x86_64" ;;
    aarch64 | arm64)  echo "aarch64" ;;
    *) echo "unknown" ;;
  esac
}

PLATFORM=$(detect_platform)
ARCH=$(detect_arch)

BIN_SUFFIX=""
if [ "$PLATFORM" = "windows" ]; then
  BIN_SUFFIX=".exe"
fi

# Download from GitHub releases
download_from_github() {
  local target_dir="$1"

  if [ "$PLATFORM" = "unknown" ] || [ "$ARCH" = "unknown" ]; then
    echo "Error: Unsupported platform ($PLATFORM) or architecture ($ARCH)"
    return 1
  fi

  local asset_base="${AGENT_BIN_NAME}-${PLATFORM}-${ARCH}"
  local archive_ext="tar.gz"
  if [ "$PLATFORM" = "windows" ]; then
    archive_ext="zip"
  fi

  # Get latest release tag
  local release_tag=""
  if command -v gh &> /dev/null; then
    release_tag=$(gh release view --repo "$GITHUB_REPO" --json tagName --jq '.tagName' 2>/dev/null || true)
  fi
  if [ -z "$release_tag" ]; then
    release_tag=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" 2>/dev/null \
      | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
  fi
  if [ -z "$release_tag" ]; then
    echo "Error: Could not determine latest release tag"
    return 1
  fi

  local url="https://github.com/${GITHUB_REPO}/releases/download/${release_tag}/${asset_base}.${archive_ext}"
  echo "Downloading ${asset_base}.${archive_ext} (${release_tag})..."

  mkdir -p "$target_dir"
  local archive="$target_dir/${asset_base}.${archive_ext}"

  if command -v curl &> /dev/null; then
    curl -fsSL -o "$archive" "$url"
  elif command -v wget &> /dev/null; then
    wget -q -O "$archive" "$url"
  else
    echo "Error: Neither curl nor wget is available"
    return 1
  fi

  # Extract
  if [ "$archive_ext" = "tar.gz" ]; then
    tar -xzf "$archive" -C "$target_dir"
  elif command -v unzip &> /dev/null; then
    unzip -o "$archive" -d "$target_dir"
  else
    echo "Error: No unzip tool available"
    return 1
  fi
  rm -f "$archive"

  local bin="$target_dir/${AGENT_BIN_NAME}${BIN_SUFFIX}"
  if [ -f "$bin" ]; then
    chmod +x "$bin"
    echo "Downloaded: $bin"
    return 0
  fi
  echo "Error: Binary not found after extraction"
  return 1
}

# Resolve binary path
resolve_binary() {
  # Priority 1: Command line argument
  if [ -n "$1" ] && [ -f "$1" ]; then
    echo "$1"
    return 0
  fi

  # Priority 2: Environment variable
  if [ -n "$NEVOFLUX_AGENT_BIN" ] && [ -f "$NEVOFLUX_AGENT_BIN" ]; then
    echo "$NEVOFLUX_AGENT_BIN"
    return 0
  fi

  # Priority 3: PATH lookup
  local in_path
  in_path=$(which "$AGENT_BIN_NAME" 2>/dev/null || true)
  if [ -n "$in_path" ] && [ -f "$in_path" ]; then
    echo "$in_path"
    return 0
  fi

  # Priority 4: Local development build
  local dev_bin="$PROJECT_ROOT/target/release/${AGENT_BIN_NAME}${BIN_SUFFIX}"
  if [ -f "$dev_bin" ]; then
    echo "$dev_bin"
    return 0
  fi

  # Priority 5: Download from GitHub releases
  local dl_dir="$PROJECT_ROOT/build/native-host"
  local dl_bin="$dl_dir/${AGENT_BIN_NAME}${BIN_SUFFIX}"
  if [ -f "$dl_bin" ]; then
    echo "$dl_bin"
    return 0
  fi

  echo "Binary not found locally. Downloading from GitHub releases..." >&2
  if download_from_github "$dl_dir"; then
    echo "$dl_bin"
    return 0
  fi

  return 1
}

echo "[1/3] Locating agent binary..."
AGENT_BIN=$(resolve_binary "$1")

if [ -z "$AGENT_BIN" ] || [ ! -f "$AGENT_BIN" ]; then
  echo "Error: Native agent binary not found"
  echo ""
  echo "Options:"
  echo "  1. Specify path:  ./setup.sh /path/to/${AGENT_BIN_NAME}"
  echo "  2. Build locally: cd $PROJECT_ROOT && cargo build --release"
  echo "  3. Set env var:   NEVOFLUX_AGENT_BIN=/path/to/binary ./setup.sh"
  exit 1
fi

# Convert to absolute path
if [[ "$AGENT_BIN" != /* ]]; then
  AGENT_BIN="$(cd "$(dirname "$AGENT_BIN")" && pwd)/$(basename "$AGENT_BIN")"
fi

echo "  Binary: $AGENT_BIN"

AGENT_DIR="$(dirname "$AGENT_BIN")"
if [ -d "$AGENT_DIR/models" ]; then
  echo "  Models: $AGENT_DIR/models"
fi
echo ""

# Register native messaging host
echo "[2/3] Registering native messaging host..."

case "$PLATFORM" in
  linux)
    MANIFEST_DIR="$HOME/.mozilla/native-messaging-hosts"
    ;;
  macos)
    MANIFEST_DIR="$HOME/Library/Application Support/Mozilla/NativeMessagingHosts"
    ;;
  windows)
    MANIFEST_DIR="$APPDATA/Mozilla/NativeMessagingHosts"
    ;;
  *)
    echo "Error: Unsupported platform"
    exit 1
    ;;
esac

mkdir -p "$MANIFEST_DIR"

MANIFEST_FILE="$MANIFEST_DIR/${MANIFEST_NAME}.json"
cat > "$MANIFEST_FILE" <<EOF
{
  "name": "$MANIFEST_NAME",
  "description": "NevoFlux AI Agent Native Messaging Host",
  "path": "$AGENT_BIN",
  "type": "stdio",
  "allowed_extensions": ["$EXTENSION_ID"]
}
EOF

echo "  Manifest: $MANIFEST_FILE"

# Windows: register in Registry
if [ "$PLATFORM" = "windows" ]; then
  WIN_MANIFEST="$(cygpath -w "$MANIFEST_FILE" 2>/dev/null || echo "$MANIFEST_FILE")"
  REG_KEY="HKCU\\Software\\Mozilla\\NativeMessagingHosts\\${MANIFEST_NAME}"
  reg add "$REG_KEY" /ve /t REG_SZ /d "$WIN_MANIFEST" /f > /dev/null 2>&1 \
    && echo "  Registry: $REG_KEY" \
    || echo "  Warning: Failed to create registry key"
fi

echo ""

# Verify
echo "[3/3] Verifying..."
if "$AGENT_BIN" --help > /dev/null 2>&1; then
  echo "  Agent is executable: OK"
else
  echo "  Warning: Could not execute agent. Try: chmod +x $AGENT_BIN"
fi

echo ""
echo "=== Setup Complete ==="
echo ""
echo "Configure API keys in:"
case "$PLATFORM" in
  windows)
    echo "  %APPDATA%\\nevoflux\\config.toml"
    ;;
  *)
    echo "  ~/.config/nevoflux/config.toml"
    ;;
esac
echo ""
