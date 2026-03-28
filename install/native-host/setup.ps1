# Native Messaging Host Setup for NevoFlux Agent (Windows)
#
# Binary resolution order:
#   1. -BinaryPath parameter
#   2. NEVOFLUX_AGENT_BIN environment variable
#   3. PATH lookup
#   4. Local development build: ..\..\target\release\nevoflux-agent.exe
#   5. GitHub release download (latest from dorisgyl/nevoflux-agent)

param(
    [string]$BinaryPath = ""
)

$ErrorActionPreference = "Stop"

$AgentBinName = "nevoflux-agent.exe"
$ExtensionId = "agent@nevoflux.com"
$ManifestName = "com.nevoflux.agent"
$GitHubRepo = "dorisgyl/nevoflux-agent"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = (Resolve-Path "$ScriptDir\..\..").Path

Write-Host "=== NevoFlux Native Messaging Host Setup ===" -ForegroundColor Cyan
Write-Host ""

# Detect architecture
function Get-PlatformArch {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        "X64"   { return "x86_64" }
        "Arm64" { return "aarch64" }
        default {
            # Fallback to PROCESSOR_ARCHITECTURE
            $envArch = $env:PROCESSOR_ARCHITECTURE
            if ($envArch -eq "AMD64") { return "x86_64" }
            if ($envArch -eq "ARM64") { return "aarch64" }
            return "unknown"
        }
    }
}

# Download from GitHub releases
function Download-FromGitHub {
    param([string]$TargetDir)

    $arch = Get-PlatformArch
    if ($arch -eq "unknown") {
        Write-Host "Error: Unsupported architecture" -ForegroundColor Red
        return $false
    }

    $assetName = "nevoflux-agent-windows-${arch}.zip"

    # Get latest release tag
    $releaseTag = $null
    try {
        $response = Invoke-RestMethod -Uri "https://api.github.com/repos/$GitHubRepo/releases/latest" -ErrorAction Stop
        $releaseTag = $response.tag_name
    } catch {
        Write-Host "Error: Could not determine latest release tag" -ForegroundColor Red
        return $false
    }

    $url = "https://github.com/$GitHubRepo/releases/download/$releaseTag/$assetName"
    Write-Host "Downloading $assetName ($releaseTag)..."

    New-Item -ItemType Directory -Force -Path $TargetDir | Out-Null
    $archivePath = Join-Path $TargetDir $assetName

    try {
        Invoke-WebRequest -Uri $url -OutFile $archivePath -ErrorAction Stop
    } catch {
        Write-Host "Error: Download failed: $_" -ForegroundColor Red
        return $false
    }

    # Extract
    Write-Host "Extracting..."
    Expand-Archive -Path $archivePath -DestinationPath $TargetDir -Force
    Remove-Item $archivePath -Force

    $bin = Join-Path $TargetDir $AgentBinName
    if (Test-Path $bin) {
        Write-Host "Downloaded: $bin"
        return $true
    }
    Write-Host "Error: Binary not found after extraction" -ForegroundColor Red
    return $false
}

# Resolve binary path
function Resolve-AgentBinary {
    # Priority 1: -BinaryPath parameter
    if ($BinaryPath -and (Test-Path $BinaryPath)) {
        return (Resolve-Path $BinaryPath).Path
    }

    # Priority 2: Environment variable
    $envBin = $env:NEVOFLUX_AGENT_BIN
    if ($envBin -and (Test-Path $envBin)) {
        return (Resolve-Path $envBin).Path
    }

    # Priority 3: PATH lookup
    $inPath = Get-Command $AgentBinName -ErrorAction SilentlyContinue
    if ($inPath) {
        return $inPath.Source
    }

    # Priority 4: Local development build
    $devBin = Join-Path $ProjectRoot "target\release\$AgentBinName"
    if (Test-Path $devBin) {
        return (Resolve-Path $devBin).Path
    }

    # Priority 5: Download from GitHub releases
    $dlDir = Join-Path $ProjectRoot "build\native-host"
    $dlBin = Join-Path $dlDir $AgentBinName
    if (Test-Path $dlBin) {
        return (Resolve-Path $dlBin).Path
    }

    Write-Host "Binary not found locally. Downloading from GitHub releases..." -ForegroundColor Yellow
    if (Download-FromGitHub -TargetDir $dlDir) {
        if (Test-Path $dlBin) {
            return (Resolve-Path $dlBin).Path
        }
    }

    return $null
}

Write-Host "[1/3] Locating agent binary..."
$AgentBin = Resolve-AgentBinary

if (-not $AgentBin -or -not (Test-Path $AgentBin)) {
    Write-Host "Error: Native agent binary not found" -ForegroundColor Red
    Write-Host ""
    Write-Host "Options:"
    Write-Host "  1. Specify path:  .\setup.ps1 -BinaryPath C:\path\to\$AgentBinName"
    Write-Host "  2. Build locally: cd $ProjectRoot; cargo build --release"
    Write-Host "  3. Set env var:   `$env:NEVOFLUX_AGENT_BIN = 'C:\path\to\$AgentBinName'"
    exit 1
}

$AgentDir = Split-Path -Parent $AgentBin
Write-Host "  Binary: $AgentBin"
if (Test-Path (Join-Path $AgentDir "models")) {
    Write-Host "  Models: $(Join-Path $AgentDir 'models')"
}
Write-Host ""

# Register native messaging host
Write-Host "[2/3] Registering native messaging host..."

$ManifestDir = Join-Path $env:APPDATA "Mozilla\NativeMessagingHosts"
New-Item -ItemType Directory -Force -Path $ManifestDir | Out-Null

$ManifestFile = Join-Path $ManifestDir "$ManifestName.json"
$escapedPath = $AgentBin -replace '\\', '\\\\'

@"
{
  "name": "$ManifestName",
  "description": "NevoFlux AI Agent Native Messaging Host",
  "path": "$escapedPath",
  "type": "stdio",
  "allowed_extensions": ["$ExtensionId"]
}
"@ | Out-File -FilePath $ManifestFile -Encoding utf8

Write-Host "  Manifest: $ManifestFile"

# Register in Windows Registry (Mozilla path)
$RegPath = "HKCU:\Software\Mozilla\NativeMessagingHosts\$ManifestName"
try {
    New-Item -Path $RegPath -Force | Out-Null
    Set-ItemProperty -Path $RegPath -Name "(Default)" -Value $ManifestFile
    Write-Host "  Registry: $RegPath"
} catch {
    Write-Host "  Warning: Failed to create registry key: $_" -ForegroundColor Yellow
}

Write-Host ""

# Verify
Write-Host "[3/3] Verifying..."
try {
    $null = & $AgentBin --help 2>&1
    Write-Host "  Agent is executable: OK" -ForegroundColor Green
} catch {
    Write-Host "  Warning: Could not execute agent" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "=== Setup Complete ===" -ForegroundColor Green
Write-Host ""
Write-Host "Configure API keys in:"
Write-Host "  $env:APPDATA\nevoflux\config.toml"
Write-Host ""
