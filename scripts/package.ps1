# Builds a release and assembles the portable folder and its zip:
#   dist/BestPDF/{best-pdf.exe, pdfium.dll, PDFIUM-LICENSE.txt, licenses/}
#   dist/BestPDF-v<version>-win-x64.zip
$ErrorActionPreference = 'Stop'
$Root = Split-Path $PSScriptRoot -Parent
& (Join-Path $PSScriptRoot 'fetch-pdfium.ps1')
cargo build --release --manifest-path (Join-Path $Root 'Cargo.toml')
if ($LASTEXITCODE) { exit $LASTEXITCODE }

# Start clean so nothing left over (e.g. signatures or settings from running
# the app out of dist) ends up in the zip.
$Out = Join-Path $Root 'dist\BestPDF'
if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
New-Item -ItemType Directory -Force $Out | Out-Null
Copy-Item (Join-Path $Root 'target\release\best-pdf.exe') $Out
Copy-Item (Join-Path $Root 'vendor\pdfium\bin\pdfium.dll') $Out
Copy-Item (Join-Path $Root 'vendor\pdfium\LICENSE') (Join-Path $Out 'PDFIUM-LICENSE.txt')
# Licenses of the libraries built into pdfium.dll.
Copy-Item (Join-Path $Root 'vendor\pdfium\licenses') (Join-Path $Out 'licenses') -Recurse

$Version = (Select-String -Path (Join-Path $Root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$Zip = Join-Path $Root "dist\BestPDF-v$Version-win-x64.zip"
if (Test-Path $Zip) { Remove-Item -Force $Zip }
Compress-Archive -Path $Out -DestinationPath $Zip
Get-ChildItem $Out | Format-Table Name, @{ n = 'KB'; e = { [math]::Round($_.Length / 1KB) } }
Write-Host "Zip: $Zip ($([math]::Round((Get-Item $Zip).Length / 1KB)) KB)"
