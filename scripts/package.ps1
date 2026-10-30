# Builds a release and assembles the portable folder: dist/BestPDF/{best-pdf.exe, pdfium.dll}
$ErrorActionPreference = 'Stop'
$Root = Split-Path $PSScriptRoot -Parent
& (Join-Path $PSScriptRoot 'fetch-pdfium.ps1')
cargo build --release --manifest-path (Join-Path $Root 'Cargo.toml')
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$Out = Join-Path $Root 'dist\BestPDF'
New-Item -ItemType Directory -Force $Out | Out-Null
Copy-Item (Join-Path $Root 'target\release\best-pdf.exe') $Out
Copy-Item (Join-Path $Root 'vendor\pdfium\bin\pdfium.dll') $Out
Copy-Item (Join-Path $Root 'vendor\pdfium\LICENSE') (Join-Path $Out 'PDFIUM-LICENSE.txt')
Get-ChildItem $Out | Format-Table Name, @{ n = 'KB'; e = { [math]::Round($_.Length / 1KB) } }
