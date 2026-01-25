# Native Messaging Host Setup

This directory contains configuration files and setup scripts for installing the NevoFlux Agent as a native messaging host for browser extensions.

## Overview

Native Messaging allows browser extensions to communicate with native applications installed on the user's computer. The NevoFlux Agent uses this mechanism to receive commands from the browser extension and execute AI-powered computer control tasks.

## Files

- `com.nevoflux.agent.json.template` - Manifest template for the native messaging host
- `setup.sh` - Setup script for Linux and macOS
- `setup.ps1` - Setup script for Windows

## Installation

### Linux/macOS

```bash
# Basic usage (uses default paths)
./setup.sh

# With custom parameters
./setup.sh /path/to/nevoflux your_extension_id chrome

# For Firefox
./setup.sh /path/to/nevoflux your_extension_id firefox

# For Chromium (Linux only)
./setup.sh /path/to/nevoflux your_extension_id chromium
```

#### Parameters

1. `BINARY_PATH` - Path to the nevoflux binary (default: searches PATH or uses `target/release/nevoflux`)
2. `EXTENSION_ID` - Chrome extension ID (required for production use)
3. `BROWSER` - Target browser: `chrome`, `firefox`, or `chromium` (default: `chrome`)

#### Supported Paths

**macOS:**
- Chrome: `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/`
- Firefox: `~/Library/Application Support/Mozilla/NativeMessagingHosts/`

**Linux:**
- Chrome: `~/.config/google-chrome/NativeMessagingHosts/`
- Chromium: `~/.config/chromium/NativeMessagingHosts/`
- Firefox: `~/.mozilla/native-messaging-hosts/`

### Windows

Run PowerShell as Administrator:

```powershell
# Basic usage
.\setup.ps1

# With custom parameters
.\setup.ps1 -BinaryPath "C:\Program Files\NevoFlux\nevoflux.exe" -ExtensionId "your_extension_id"
```

#### Parameters

- `-BinaryPath` - Path to nevoflux.exe (default: `C:\Program Files\NevoFlux\nevoflux.exe`)
- `-ExtensionId` - Chrome extension ID (required for production use)

#### What the Script Does

1. Creates the manifest directory at `%LOCALAPPDATA%\NevoFlux\`
2. Generates `com.nevoflux.agent.json` with the correct binary path
3. Registers the native messaging host in the Windows Registry at:
   `HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.nevoflux.agent`

## Manifest Format

The native messaging host manifest follows the Chrome/Firefox specification:

```json
{
  "name": "com.nevoflux.agent",
  "description": "NevoFlux Agent - AI-powered computer control",
  "path": "/absolute/path/to/nevoflux",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://extension_id_here/"
  ]
}
```

## Troubleshooting

### Extension Cannot Connect

1. Verify the binary path in the manifest is correct and the file exists
2. Ensure the binary has execute permissions (`chmod +x` on Linux/macOS)
3. Check that the extension ID matches exactly (including the trailing `/`)
4. Verify the manifest file is in the correct location for your browser

### Permission Denied

- Linux/macOS: Make sure the setup script is executable (`chmod +x setup.sh`)
- Windows: Run PowerShell as Administrator

### Manifest Not Found

Verify the manifest file exists in the correct location:

```bash
# Linux (Chrome)
cat ~/.config/google-chrome/NativeMessagingHosts/com.nevoflux.agent.json

# macOS (Chrome)
cat ~/Library/Application\ Support/Google/Chrome/NativeMessagingHosts/com.nevoflux.agent.json
```

```powershell
# Windows
Get-Content "$env:LOCALAPPDATA\NevoFlux\com.nevoflux.agent.json"
```

## Uninstallation

### Linux/macOS

```bash
# Chrome
rm ~/.config/google-chrome/NativeMessagingHosts/com.nevoflux.agent.json

# Firefox
rm ~/.mozilla/native-messaging-hosts/com.nevoflux.agent.json
```

### Windows

```powershell
Remove-Item "$env:LOCALAPPDATA\NevoFlux" -Recurse -Force
Remove-Item "HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.nevoflux.agent" -Force
```

## Security Considerations

- The native messaging host only accepts connections from the specified extension ID
- The binary path must be absolute to prevent path hijacking
- On Windows, the registry entry points to the manifest, not the binary directly
- Always verify the extension ID before deploying to production
