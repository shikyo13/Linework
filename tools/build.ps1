<#
Builds Linework from this repository with the toolchain the shipped builds use,
then starts the candidate with a disposable profile. The installed editor, its
settings and its session are not touched.

  tools\build.ps1            build, then run the candidate on docs\try-the-effects.md
  tools\build.ps1 -NoRun     build only
  tools\build.ps1 -Check     type-check only
  tools\build.ps1 -Open X    run the candidate on file X
#>
param([switch]$NoRun, [switch]$Check, [string]$Open)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $repo
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $repo 'target' }
if ($Check) {
    cargo +1.94 check --locked --offline --bin linework
    exit $LASTEXITCODE
}
cargo +1.94 build --release --locked --offline --bin linework
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$exe = Join-Path $env:CARGO_TARGET_DIR 'release\linework.exe'
Write-Host "Built   $exe"
Write-Host ("SHA-256 " + (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash)
if ($NoRun) { exit 0 }
$env:LINEWORK_RUNTIME = Join-Path $repo 'runtime'
$env:LINEWORK_PROFILE = Join-Path $repo '.scratch\profile'
New-Item -ItemType Directory -Force -Path $env:LINEWORK_PROFILE | Out-Null
if (-not $Open) {
    # A copy, so trying things out never changes the tracked file.
    $Open = Join-Path $repo '.scratch\try-the-effects.md'
    Copy-Item -LiteralPath (Join-Path $repo 'docs\try-the-effects.md') -Destination $Open -Force
}
& $exe $Open
