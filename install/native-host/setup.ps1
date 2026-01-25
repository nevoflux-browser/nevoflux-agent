# Native Messaging Host Setup for NevoFlux Agent (Windows)
param(
    [string]$BinaryPath = "C:\Program Files\NevoFlux\nevoflux.exe",
    [string]$ExtensionId = "placeholder_extension_id"
)

$ManifestPath = "$env:LOCALAPPDATA\NevoFlux\com.nevoflux.agent.json"
$RegistryPath = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.nevoflux.agent"

# Create directory
New-Item -ItemType Directory -Force -Path (Split-Path $ManifestPath)

# Generate manifest
@"
{
  "name": "com.nevoflux.agent",
  "description": "NevoFlux Agent - AI-powered computer control",
  "path": "$($BinaryPath -replace '\\', '\\\\')",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://$ExtensionId/"
  ]
}
"@ | Out-File -FilePath $ManifestPath -Encoding utf8

# Register in registry
New-Item -Path $RegistryPath -Force | Out-Null
Set-ItemProperty -Path $RegistryPath -Name "(Default)" -Value $ManifestPath

Write-Host "Native messaging host installed successfully!"
Write-Host "Manifest: $ManifestPath"
