# Fetches the yazi file manager into dep\yazi\<arch>\ for the package build.
#
# CascadiaPackage.wapproj links dep\yazi\<arch>\yazi.exe and ya.exe into the
# package root, and TerminalPage::_DetectYaziPath() also looks there when the
# dev tree is run unpackaged. Both files are committed, so this script is only
# needed to (re)populate a tree that has them missing, or to bump the version.
#
# Usage:
#   tools\fetch-yazi.ps1              # fill in whatever is missing
#   tools\fetch-yazi.ps1 -Force       # re-download both architectures
#   tools\fetch-yazi.ps1 -Version 26.9.1
#
# The upstream release zips carry a per-target root folder
# (yazi-x86_64-pc-windows-msvc\yazi.exe); this script flattens that away so the
# layout matches what the wapproj expects.

[CmdletBinding()]
param (
    [string]$Version = '26.9.1',

    # Where the flattened <arch>\yazi.exe layout lives. Defaults to the repo's
    # dep\yazi, resolved from this script's own folder.
    [string]$Destination,

    [switch]$Force
)

$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar makes a 17 MB download crawl on 5.1.
$ProgressPreference = 'SilentlyContinue'

if (-not $Destination) {
    $Destination = Join-Path (Split-Path -Parent $PSScriptRoot) 'dep\yazi'
}

# dep\yazi folder name -> Rust target triple used in the release asset name.
$targets = [ordered]@{
    x64   = 'x86_64-pc-windows-msvc'
    arm64 = 'aarch64-pc-windows-msvc'
}

function Get-YaziArch {
    param ([string]$Arch, [string]$Triple)

    $outDir = Join-Path $Destination $Arch
    $yaziExe = Join-Path $outDir 'yazi.exe'
    $yaExe = Join-Path $outDir 'ya.exe'

    if (-not $Force -and (Test-Path -LiteralPath $yaziExe) -and (Test-Path -LiteralPath $yaExe)) {
        Write-Host "yazi ($Arch): already present, skipping"
        return
    }

    $asset = "yazi-$Triple.zip"
    $url = "https://github.com/sxyazi/yazi/releases/download/v$Version/$asset"
    $workDir = Join-Path ([System.IO.Path]::GetTempPath()) ("fetch-yazi-" + [guid]::NewGuid().ToString('N'))
    $zipPath = Join-Path $workDir $asset
    $extractDir = Join-Path $workDir 'x'

    try {
        New-Item -ItemType Directory -Path $workDir -Force | Out-Null
        Write-Host "yazi ($Arch): downloading $url"
        Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing

        Expand-Archive -LiteralPath $zipPath -DestinationPath $extractDir -Force

        # Find the binaries rather than trusting the release's root folder name,
        # which upstream has changed before.
        $srcYazi = Get-ChildItem -LiteralPath $extractDir -Filter 'yazi.exe' -Recurse -File |
            Select-Object -First 1
        $srcYa = Get-ChildItem -LiteralPath $extractDir -Filter 'ya.exe' -Recurse -File |
            Select-Object -First 1
        if (-not $srcYazi -or -not $srcYa) {
            throw "$asset did not contain both yazi.exe and ya.exe"
        }

        New-Item -ItemType Directory -Path $outDir -Force | Out-Null
        Copy-Item -LiteralPath $srcYazi.FullName -Destination $yaziExe -Force
        Copy-Item -LiteralPath $srcYa.FullName -Destination $yaExe -Force
        Write-Host "yazi ($Arch): installed to $outDir"
    }
    finally {
        # Temp only: nothing under dep\yazi is touched on the failure path
        # beyond the copy above, which -Force already made intentional.
        if (Test-Path -LiteralPath $workDir) {
            Remove-Item -LiteralPath $workDir -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

foreach ($arch in $targets.Keys) {
    Get-YaziArch -Arch $arch -Triple $targets[$arch]
}

Write-Host "yazi v$Version ready under $Destination"
