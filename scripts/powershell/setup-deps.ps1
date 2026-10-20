<#
.SYNOPSIS
    Prepare Windows build dependencies for SlintYT.

.DESCRIPTION
    Downloads the latest libmpv development build from
    shinchiro/mpv-winbuild-cmake, extracts libmpv-2.dll, and synthesises an
    import library (mpv.lib) suitable for the MSVC linker via dumpbin + lib.

    Output files land in <repo>\deps:
        deps\libmpv-2.dll
        deps\mpv.lib

    The script is idempotent: if both outputs already exist it returns
    immediately. Pass -Force to redownload and regenerate.

.PARAMETER Force
    Rebuild even if deps\libmpv-2.dll and deps\mpv.lib already exist.
#>
[CmdletBinding()]
param(
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Split-Path -Parent (Split-Path -Parent $ScriptDir)
$DepsDir   = Join-Path $RepoRoot 'deps'
$DllOut   = Join-Path $DepsDir  'libmpv-2.dll'
$LibOut   = Join-Path $DepsDir  'mpv.lib'

if (-not $Force -and (Test-Path $DllOut) -and (Test-Path $LibOut)) {
    Write-Host "[setup-deps] deps already present - skipping (use -Force to refresh)"
    exit 0
}

function Find-SevenZip {
    $cmd = Get-Command '7z' -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    foreach ($p in @(
        "$env:ProgramFiles\7-Zip\7z.exe",
        "${env:ProgramFiles(x86)}\7-Zip\7z.exe"
    )) {
        if ($p -and (Test-Path $p)) { return $p }
    }
    throw "7-Zip not found. Install it (e.g. 'winget install -e --id 7zip.7zip') and re-run."
}

function Find-VcVars {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) {
        throw "vswhere.exe not found. Install Visual Studio Build Tools with the C++ workload."
    }
    $vsInstall = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
    if (-not $vsInstall) {
        throw "No Visual Studio installation with the MSVC x64 toolchain was found."
    }
    $vcvars = Join-Path $vsInstall 'VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) {
        throw "vcvars64.bat missing under '$vsInstall'."
    }
    return $vcvars
}

function Get-LatestMpvDevAsset {
    # Walk recent releases until one carries an mpv-dev x86_64 asset.
    $headers = @{
        'User-Agent' = 'slintyt-setup-deps'
        'Accept'     = 'application/vnd.github+json'
    }
    if ($env:GH_TOKEN)     { $headers['Authorization'] = "Bearer $env:GH_TOKEN" }
    elseif ($env:GITHUB_TOKEN) { $headers['Authorization'] = "Bearer $env:GITHUB_TOKEN" }

    $uri = 'https://api.github.com/repos/shinchiro/mpv-winbuild-cmake/releases?per_page=10'
    $releases = Invoke-RestMethod -Uri $uri -Headers $headers
    foreach ($r in $releases) {
        $asset = $r.assets | Where-Object { $_.name -like 'mpv-dev-x86_64-2*-git-*.7z' } | Select-Object -First 1
        if ($asset) { return $asset }
    }
    throw "No mpv-dev-x86_64 archive found in the 10 most recent shinchiro/mpv-winbuild-cmake releases."
}

New-Item -ItemType Directory -Force -Path $DepsDir | Out-Null
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("slintyt-mpvdev-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $work | Out-Null

try {
    $sevenZip = Find-SevenZip
    $vcvars   = Find-VcVars

    Write-Host "[setup-deps] Querying shinchiro/mpv-winbuild-cmake for latest mpv-dev build..."
    $asset = Get-LatestMpvDevAsset
    $archive = Join-Path $work $asset.name
    Write-Host "[setup-deps] Downloading $($asset.name) ($([math]::Round($asset.size/1MB,1)) MB)..."
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $archive -UseBasicParsing

    Write-Host "[setup-deps] Extracting archive..."
    & $sevenZip x $archive "-o$work" -y | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "7-Zip extraction failed (exit $LASTEXITCODE)." }

    $dll = Get-ChildItem $work -Recurse -Filter 'libmpv-2.dll' | Select-Object -First 1
    if (-not $dll) { throw "libmpv-2.dll not present in extracted archive." }

    $defPath     = Join-Path $work 'mpv.def'
    $exportsFile = Join-Path $work 'exports.txt'
    $libTmp      = Join-Path $work 'mpv.lib'

    Write-Host "[setup-deps] Dumping DLL exports via MSVC dumpbin..."
    cmd /c "`"$vcvars`" >nul && dumpbin /exports `"$($dll.FullName)`" > `"$exportsFile`""
    if ($LASTEXITCODE -ne 0) { throw "dumpbin failed (exit $LASTEXITCODE)." }

    $exports = Get-Content $exportsFile | ForEach-Object {
        if ($_ -match '^\s+\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]+\s+(\S+)') { $matches[1] }
    }
    if (-not $exports) { throw "Failed to parse exports from dumpbin output." }
    Write-Host "[setup-deps] Found $($exports.Count) exports."
    (@('EXPORTS') + $exports) | Set-Content -Path $defPath -Encoding ASCII

    Write-Host "[setup-deps] Generating mpv.lib import library..."
    cmd /c "`"$vcvars`" >nul && lib /nologo /def:`"$defPath`" /name:libmpv-2.dll /out:`"$libTmp`" /MACHINE:X64"
    if ($LASTEXITCODE -ne 0) { throw "lib.exe failed (exit $LASTEXITCODE)." }
    if (-not (Test-Path $libTmp)) { throw "Expected mpv.lib was not produced." }

    Copy-Item $dll.FullName $DllOut -Force
    Copy-Item $libTmp       $LibOut -Force

    Write-Host "[setup-deps] Done."
    Write-Host "    $DllOut"
    Write-Host "    $LibOut"
}
finally {
    if (Test-Path $work) {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}
