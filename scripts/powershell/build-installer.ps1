<#
.SYNOPSIS
    Build the Windows SlintYT installer.

.DESCRIPTION
    Compiles scripts/installer/slintyt.iss with Inno Setup using an already-built
    SlintYT executable and staged libmpv-2.dll.

.PARAMETER Version
    Optional installer version. Defaults to the version in Cargo.toml.

.PARAMETER BuildDir
    Directory containing slintyt.exe and libmpv-2.dll. The installer is emitted
    into this same directory.

.PARAMETER OutputSuffix
    Optional suffix inserted before "-Setup" in the installer file name.
#>
[CmdletBinding()]
param(
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$BuildDir,
    [string]$OutputSuffix = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Split-Path -Parent (Split-Path -Parent $ScriptDir)
$InstallerScript = Join-Path $RepoRoot 'scripts\installer\slintyt.iss'
if ([System.IO.Path]::IsPathRooted($BuildDir)) {
    $ResolvedBuildDir = [System.IO.Path]::GetFullPath($BuildDir)
}
else {
    $ResolvedBuildDir = [System.IO.Path]::GetFullPath((Join-Path $RepoRoot $BuildDir))
}
$ExePath = Join-Path $ResolvedBuildDir 'slintyt.exe'
$MpvDllPath = Join-Path $ResolvedBuildDir 'libmpv-2.dll'

if (-not $Version) {
    $manifest = Get-Content (Join-Path $RepoRoot 'Cargo.toml')
    $versionLine = $manifest | Where-Object { $_ -match '^\s*version\s*=\s*"([^"]+)"' } | Select-Object -First 1
    if (-not $versionLine) {
        throw 'Could not find package version in Cargo.toml.'
    }
    $Version = [regex]::Match($versionLine, '^\s*version\s*=\s*"([^"]+)"').Groups[1].Value
}

function Find-InnoCompiler {
    $cmd = Get-Command 'ISCC.exe' -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }

    foreach ($path in @(
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
        (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
    )) {
        if ($path -and (Test-Path $path)) { return $path }
    }

    throw "Inno Setup compiler not found. Install Inno Setup 6.3 or newer, then re-run this script."
}

foreach ($required in @($ExePath, $MpvDllPath, $InstallerScript)) {
    if (-not (Test-Path $required)) {
        throw "Required file missing: $required"
    }
}

$iscc = Find-InnoCompiler
$outputBaseFilename = "SlintYT-$Version$OutputSuffix-Setup"
New-Item -ItemType Directory -Force -Path $ResolvedBuildDir | Out-Null

& $iscc `
    "/DAppVersion=$Version" `
    "/DAppSourceDir=$ResolvedBuildDir" `
    "/DAppOutputDir=$ResolvedBuildDir" `
    "/DAppOutputBaseFilename=$outputBaseFilename" `
    $InstallerScript
if ($LASTEXITCODE -ne 0) {
    throw "Inno Setup failed with exit code $LASTEXITCODE."
}

$output = Join-Path $ResolvedBuildDir "$outputBaseFilename.exe"
Write-Host "Installer built:"
Write-Host "    $output"
