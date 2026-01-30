#!/bin/bash
# Native Messaging Host Setup for NevoFlux Agent
# Supports Chrome and Firefox on Linux/macOS

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY_PATH="${1:-$(which nevoflux-agent 2>/dev/null || echo "$SCRIPT_DIR/../../target/release/nevoflux-agent")}"
EXTENSION_ID="${2:-agent@nevoflux.com}"
BROWSER="${3:-firefox}"

# Convert to absolute path
if [[ "$BINARY_PATH" != /* ]]; then
    BINARY_PATH="$(cd "$(dirname "$BINARY_PATH")" && pwd)/$(basename "$BINARY_PATH")"
fi

echo "NevoFlux Native Messaging Host Setup"
echo "====================================="
echo "Binary: $BINARY_PATH"
echo "Extension ID: $EXTENSION_ID"
echo "Browser: $BROWSER"

# Determine host directory based on OS and browser
if [[ "$OSTYPE" == "darwin"* ]]; then
    case "$BROWSER" in
        chrome)
            HOST_DIR="$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts"
            ;;
        firefox)
            HOST_DIR="$HOME/Library/Application Support/Mozilla/NativeMessagingHosts"
            ;;
    esac
elif [[ "$OSTYPE" == "linux-gnu"* ]]; then
    case "$BROWSER" in
        chrome)
            HOST_DIR="$HOME/.config/google-chrome/NativeMessagingHosts"
            ;;
        chromium)
            HOST_DIR="$HOME/.config/chromium/NativeMessagingHosts"
            ;;
        firefox)
            HOST_DIR="$HOME/.mozilla/native-messaging-hosts"
            ;;
    esac
fi

if [ -z "$HOST_DIR" ]; then
    echo "Error: Unsupported OS or browser"
    exit 1
fi

mkdir -p "$HOST_DIR"

# Generate manifest based on browser type
MANIFEST_FILE="$HOST_DIR/com.nevoflux.agent.json"

if [[ "$BROWSER" == "firefox" ]]; then
    # Firefox uses allowed_extensions
    cat > "$MANIFEST_FILE" <<EOF
{
  "name": "com.nevoflux.agent",
  "description": "NevoFlux Agent - AI-powered computer control",
  "path": "$BINARY_PATH",
  "type": "stdio",
  "allowed_extensions": ["$EXTENSION_ID"]
}
EOF
else
    # Chrome/Chromium uses allowed_origins
    cat > "$MANIFEST_FILE" <<EOF
{
  "name": "com.nevoflux.agent",
  "description": "NevoFlux Agent - AI-powered computer control",
  "path": "$BINARY_PATH",
  "type": "stdio",
  "allowed_origins": ["chrome-extension://$EXTENSION_ID/"]
}
EOF
fi

echo "Installed: $MANIFEST_FILE"
cat "$MANIFEST_FILE"
echo ""
echo "Done!"
