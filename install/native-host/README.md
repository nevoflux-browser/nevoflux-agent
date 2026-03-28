# Native Messaging Host Setup

Setup scripts to register the NevoFlux Agent as a native messaging host for NevoFlux browser (Firefox-based).

## Files

- `setup.sh` - Setup script for Linux, macOS, and Windows (MSYS/Git Bash)
- `setup.ps1` - Setup script for Windows (PowerShell)
- `com.nevoflux.agent.json.template` - Manifest template

## Binary Resolution

Both scripts resolve the agent binary in this order:

1. **Explicit path** - command line argument or `-BinaryPath` parameter
2. **Environment variable** - `NEVOFLUX_AGENT_BIN`
3. **PATH lookup** - `which nevoflux-agent`
4. **Local dev build** - `../../target/release/nevoflux-agent`
5. **GitHub release download** - latest release from `dorisgyl/nevoflux-agent`

## Usage

### Linux/macOS

```bash
# Auto-detect binary (tries all resolution methods)
./setup.sh

# Specify binary path
./setup.sh /path/to/nevoflux-agent
```

### Windows (PowerShell)

```powershell
# Auto-detect binary
.\setup.ps1

# Specify binary path
.\setup.ps1 -BinaryPath "C:\path\to\nevoflux-agent.exe"
```

## What the Scripts Do

1. Locate or download the `nevoflux-agent` binary
2. Create the native messaging manifest at the platform-specific location:
   - **Linux**: `~/.mozilla/native-messaging-hosts/com.nevoflux.agent.json`
   - **macOS**: `~/Library/Application Support/Mozilla/NativeMessagingHosts/com.nevoflux.agent.json`
   - **Windows**: `%APPDATA%\Mozilla\NativeMessagingHosts\com.nevoflux.agent.json`
3. **Windows only**: Register the manifest in the Windows Registry at `HKCU:\Software\Mozilla\NativeMessagingHosts\com.nevoflux.agent`

## Configuration

After setup, configure API keys in:

- **Linux/macOS**: `~/.config/nevoflux/config.toml`
- **Windows**: `%APPDATA%\nevoflux\config.toml`

## Uninstallation

### Linux/macOS

```bash
rm ~/.mozilla/native-messaging-hosts/com.nevoflux.agent.json
# or on macOS:
rm ~/Library/Application\ Support/Mozilla/NativeMessagingHosts/com.nevoflux.agent.json
```

### Windows

```powershell
Remove-Item "$env:APPDATA\Mozilla\NativeMessagingHosts\com.nevoflux.agent.json" -Force
Remove-Item "HKCU:\Software\Mozilla\NativeMessagingHosts\com.nevoflux.agent" -Force
```
