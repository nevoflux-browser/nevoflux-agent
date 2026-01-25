# NevoFlux Agent Browser Extension

AI-powered browser assistant with native computer control capabilities.

## Installation

### Chrome / Chromium

1. Open Chrome and navigate to `chrome://extensions/`
2. Enable **Developer mode** (toggle in the top-right corner)
3. Click **Load unpacked**
4. Select the `extension` directory from this repository
5. The extension icon should appear in your toolbar

### Firefox

1. Open Firefox and navigate to `about:debugging#/runtime/this-firefox`
2. Click **Load Temporary Add-on...**
3. Select the `manifest.json` file from the `extension` directory
4. The extension will be loaded (note: temporary add-ons are removed when Firefox closes)

For permanent Firefox installation, the extension needs to be signed by Mozilla.

## Native Messaging Setup

The extension communicates with the NevoFlux Agent daemon via native messaging. To set up:

### Linux

```bash
# Build the native messaging host
cargo build --release -p nevoflux-daemon

# Install the native messaging manifest
./target/release/nevoflux-daemon install-native-messaging
```

This creates the manifest at:
- Chrome: `~/.config/google-chrome/NativeMessagingHosts/com.nevoflux.agent.json`
- Chromium: `~/.config/chromium/NativeMessagingHosts/com.nevoflux.agent.json`
- Firefox: `~/.mozilla/native-messaging-hosts/com.nevoflux.agent.json`

### macOS

```bash
# Build the native messaging host
cargo build --release -p nevoflux-daemon

# Install the native messaging manifest
./target/release/nevoflux-daemon install-native-messaging
```

This creates the manifest at:
- Chrome: `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.nevoflux.agent.json`
- Firefox: `~/Library/Application Support/Mozilla/NativeMessagingHosts/com.nevoflux.agent.json`

### Windows

```powershell
# Build the native messaging host
cargo build --release -p nevoflux-daemon

# Install the native messaging manifest (run as Administrator)
.\target\release\nevoflux-daemon.exe install-native-messaging
```

This creates a registry entry pointing to the manifest file.

## Message Format

### Extension to Native Host

Messages sent from the extension to the native host follow this format:

```json
{
  "type": "command",
  "action": "click",
  "payload": {
    "x": 100,
    "y": 200
  }
}
```

### Native Host to Extension

Responses from the native host:

```json
{
  "type": "response",
  "success": true,
  "data": {
    "result": "action completed"
  }
}
```

### Supported Message Types

| Type | Description |
|------|-------------|
| `command` | Execute a computer control action |
| `query` | Request information from the agent |
| `status` | Check connection/agent status |

### Example Commands

```json
// Click at coordinates
{
  "type": "command",
  "action": "click",
  "payload": { "x": 100, "y": 200 }
}

// Type text
{
  "type": "command",
  "action": "type",
  "payload": { "text": "Hello, world!" }
}

// Take screenshot
{
  "type": "command",
  "action": "screenshot",
  "payload": {}
}

// Get agent status
{
  "type": "status"
}
```

## Development

### Project Structure

```
extension/
├── manifest.json      # Extension manifest (v3)
├── background.js      # Service worker for native messaging
├── content.js         # Content script (injected into pages)
├── popup.html         # Extension popup UI
├── popup.js           # Popup logic
└── icons/             # Extension icons
    ├── icon16.png
    ├── icon32.png
    ├── icon48.png
    └── icon128.png
```

### Debugging

- **Chrome**: Open `chrome://extensions/`, find NevoFlux Agent, click "Inspect views: service worker"
- **Firefox**: Open `about:debugging#/runtime/this-firefox`, find NevoFlux Agent, click "Inspect"

### Reloading

After making changes:
1. **Chrome**: Click the reload icon on the extension card at `chrome://extensions/`
2. **Firefox**: Click "Reload" at `about:debugging#/runtime/this-firefox`

## Permissions

The extension requests the following permissions:

| Permission | Purpose |
|------------|---------|
| `nativeMessaging` | Communicate with the NevoFlux Agent daemon |
| `activeTab` | Access the currently active tab |
| `scripting` | Inject scripts into web pages |
| `tabs` | Access tab information |
| `<all_urls>` | Run content scripts on all websites |

## Troubleshooting

### Native messaging not working

1. Verify the daemon is running: `nevoflux-daemon status`
2. Check the native messaging manifest is installed correctly
3. Ensure the manifest points to the correct binary path
4. Check browser console for error messages

### Extension not loading

1. Ensure manifest.json is valid JSON
2. Check for required files (background.js, content.js)
3. Verify all icon files exist

### Connection errors

1. The daemon may need to be restarted after installing native messaging
2. Browser may need to be restarted after manifest installation
