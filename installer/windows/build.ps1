# Build Relay-<version>.msi (PRD §12 M8, §10 Windows).
#
# Requires: Rust (MSVC), WiX 5.0.2 (`dotnet tool install --global wix --version 5.0.2`;
#           `wix extension add -g WixToolset.Firewall.wixext/5.0.2`).
# Optional signing (EV certificate strongly recommended, §10):
#   $env:WINDOWS_SIGN_THUMBPRINT   certificate thumbprint in the cert store
#   $env:WINDOWS_TIMESTAMP_URL     RFC 3161 timestamp server (default DigiCert)
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..\..")

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
# Engine (relay.exe) and the desktop app (relay-app.exe, Tauri; uses the
# WebView2 runtime that ships with Windows 10/11).
cargo build --release -p relay-app -p relay-ui
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
$exe = "target\release\relay.exe"
$app = "target\release\relay-app.exe"
$out = "target\installer\windows"
New-Item -ItemType Directory -Force -Path $out | Out-Null

function Sign($path) {
  if ($env:WINDOWS_SIGN_THUMBPRINT) {
    $ts = if ($env:WINDOWS_TIMESTAMP_URL) { $env:WINDOWS_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }
    signtool sign /sha1 $env:WINDOWS_SIGN_THUMBPRINT /fd sha256 /tr $ts /td sha256 $path
    if ($LASTEXITCODE -ne 0) { throw "signtool failed for $path" }
  } else {
    Write-Warning "WINDOWS_SIGN_THUMBPRINT not set; $path is unsigned (SmartScreen will warn)"
  }
}

Sign $exe
Sign $app
$msi = "$out\Relay-$version.msi"
wix build installer\windows\Relay.wxs -arch x64 -ext WixToolset.Firewall.wixext `
  -d Version=$version -d RelayExe=$exe -d RelayAppExe=$app -o $msi
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }
Sign $msi

$size = (Get-Item $msi).Length
Write-Host "$msi : $([math]::Round($size / 1MB, 1)) MB"
if ($size -gt 25MB) { throw "installer is larger than 25 MB (PRD §2 goal 7)" }
