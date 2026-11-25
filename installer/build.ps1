# Builds the per-user Windows installer (PORTING.md D115): target\installer\abhoth-<version>-x64.msi.
#
#   pwsh installer/build.ps1                 # cargo build --release --features desktop, then the MSI
#   pwsh installer/build.ps1 -NoCargo        # the MSI from the binaries already in target\release
#
# Needs the WiX v5 .NET tool (per-user, no admin): dotnet tool install --global wix --version 5.0.2
param([switch]$NoCargo)
$ErrorActionPreference = 'Stop'

$rust = Split-Path -Parent $PSScriptRoot
Push-Location $rust
try {
    if (-not $NoCargo) {
        cargo build --release --locked -p orca-cli --features desktop
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    }
    $version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' |
        Select-Object -First 1).Matches[0].Groups[1].Value
    $bin = Join-Path $rust 'target\release'
    foreach ($exe in 'abhoth.exe', 'abhoth-desktop.exe') {
        if (-not (Test-Path (Join-Path $bin $exe))) { throw "missing $bin\$exe" }
    }
    $out = Join-Path $rust 'target\installer'
    New-Item -ItemType Directory -Force $out | Out-Null
    $msi = Join-Path $out "abhoth-$version-x64.msi"
    wix build (Join-Path $PSScriptRoot 'abhoth.wxs') -arch x64 `
        -d "Version=$version" -d "BinDir=$bin" -d "IconDir=$(Join-Path $rust 'assets\abhoth\icons')" `
        -o $msi
    if ($LASTEXITCODE -ne 0) { throw "wix build failed ($LASTEXITCODE)" }
    Write-Output $msi
}
finally {
    Pop-Location
}
