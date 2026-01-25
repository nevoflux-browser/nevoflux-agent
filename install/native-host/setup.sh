#!/bin/bash
# Native Messaging Host Setup for NevoFlux Agent
# Supports Chrome and Firefox on Linux/macOS

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY_PATH="${1:-$(which nevoflux 2>/dev/null || echo "$SCRIPT_DIR/../../target/release/nevoflux")}"
EXTENSION_ID="${2:-placeholder_extension_id}"
BROWSER="${3:-chrome}"

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

# Generate manifest
MANIFEST_FILE="$HOST_DIR/com.nevoflux.agent.json"
sed -e "s|{{BINARY_PATH}}|$BINARY_PATH|g" \
    -e "s|{{EXTENSION_ID}}|$EXTENSION_ID|g" \
    "$SCRIPT_DIR/com.nevoflux.agent.json.template" > "$MANIFEST_FILE"

echo "Installed: $MANIFEST_FILE"
echo "Done!"
