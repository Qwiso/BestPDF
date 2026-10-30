# Downloads the prebuilt PDFium DLL (bblanchon/pdfium-binaries) into vendor/pdfium.
# The version is pinned to match the pdfium-render feature flag in Cargo.toml.
$ErrorActionPreference = 'Stop'
$Version = '7881'
$Root = Split-Path $PSScriptRoot -Parent
$Dest = Join-Path $Root 'vendor\pdfium'
$Dll = Join-Path $Dest 'bin\pdfium.dll'

if (Test-Path $Dll) { Write-Host "PDFium $Version already present"; exit 0 }

New-Item -ItemType Directory -Force $Dest | Out-Null
$Tgz = Join-Path $env:TEMP "pdfium-$Version-win-x64.tgz"
$Url = "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/$Version/pdfium-win-x64.tgz"
Write-Host "Downloading $Url"
Invoke-WebRequest $Url -OutFile $Tgz
tar -xzf $Tgz -C $Dest
Remove-Item $Tgz
Write-Host "PDFium $Version -> $Dll"
