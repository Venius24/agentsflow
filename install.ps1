# agentsflow installer for Windows (x86_64).
#
#   irm https://raw.githubusercontent.com/Venius24/agentsflow/master/install.ps1 | iex
#
# Downloads a release archive from GitHub, checks its SHA-256 against the
# SHA256SUMS file of the same release, and installs agentsflow.exe into a
# user folder. It does not need administrator rights, does not change your
# PATH or any system setting: it only tells you what to do.
#
# Options (as parameters when run from a file, or as environment variables
# when piped to iex):
#   -Version / $env:AGENTSFLOW_VERSION          release tag, e.g. v0.1.0 (default: latest)
#   -InstallDir / $env:AGENTSFLOW_INSTALL_DIR   target folder (default: %LOCALAPPDATA%\agentsflow\bin)
#   $env:AGENTSFLOW_DOWNLOAD_BASE               URL of a folder holding the archive and SHA256SUMS
#                                               (for a mirror or a test)
param(
    [string]$Version = $env:AGENTSFLOW_VERSION,
    [string]$InstallDir = $env:AGENTSFLOW_INSTALL_DIR
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
# Progress bars make Invoke-WebRequest very slow in Windows PowerShell 5.1.
$ProgressPreference = 'SilentlyContinue'

$repo = 'Venius24/agentsflow'
$bin = 'agentsflow'

# Windows PowerShell 5.1 may default to old TLS versions.
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch { }

# Only the 64-bit Intel/AMD build exists. It also runs on Windows on ARM under emulation.
if (-not [Environment]::Is64BitOperatingSystem) {
    throw "install.ps1: no prebuilt binary for 32-bit Windows. Build from source: https://github.com/$repo#install"
}

$target = 'x86_64-pc-windows-msvc'
$archive = "$bin-$target.zip"

if ([string]::IsNullOrWhiteSpace($Version)) { $Version = 'latest' }
if ($env:AGENTSFLOW_DOWNLOAD_BASE) {
    $base = $env:AGENTSFLOW_DOWNLOAD_BASE.TrimEnd('/')
} elseif ($Version -eq 'latest') {
    $base = "https://github.com/$repo/releases/latest/download"
} else {
    $base = "https://github.com/$repo/releases/download/$Version"
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        throw 'install.ps1: LOCALAPPDATA is not set; pass -InstallDir.'
    }
    $InstallDir = Join-Path $env:LOCALAPPDATA 'agentsflow\bin'
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("agentsflow-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Write-Host "Platform:   $target"
    Write-Host "Release:    $Version"
    Write-Host "Install to: $InstallDir"
    Write-Host ''

    Write-Host "Downloading $archive ..."
    $zipPath = Join-Path $tmp $archive
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$archive" -OutFile $zipPath
    } catch {
        throw ("install.ps1: could not download $base/$archive`n" +
            "  Either no release is published yet, this release has no build for your platform, or the network is down.`n" +
            "  Releases: https://github.com/$repo/releases`n" +
            "  To build from source instead: https://github.com/$repo#install`n" +
            "  ($($_.Exception.Message))")
    }
    $sumsPath = Join-Path $tmp 'SHA256SUMS'
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sumsPath
    } catch {
        throw "install.ps1: could not download $base/SHA256SUMS (refusing to install without a checksum). ($($_.Exception.Message))"
    }

    # SHA256SUMS lines look like "<hash>  <file>" (a leading "*" marks binary mode).
    $expected = $null
    foreach ($line in Get-Content -LiteralPath $sumsPath) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $archive) {
            $expected = $parts[0].ToLowerInvariant()
            break
        }
    }
    if (-not $expected) { throw "install.ps1: $archive is not listed in SHA256SUMS." }

    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $zipPath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "install.ps1: checksum mismatch for $archive`n  expected: $expected`n  actual:   $actual`nNothing was installed."
    }
    Write-Host "Checksum OK ($actual)"

    $extractDir = Join-Path $tmp 'x'
    Expand-Archive -LiteralPath $zipPath -DestinationPath $extractDir -Force
    $exe = Join-Path $extractDir "$bin-$target\$bin.exe"
    if (-not (Test-Path -LiteralPath $exe)) { throw "install.ps1: $bin.exe is missing from $archive." }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $dest = Join-Path $InstallDir "$bin.exe"
    try {
        Copy-Item -LiteralPath $exe -Destination $dest -Force
    } catch {
        throw "install.ps1: could not write $dest. If agentsflow is running, stop it and try again. ($($_.Exception.Message))"
    }

    Write-Host "Installed $dest"
    try { & $dest --version } catch { Write-Warning "The installed binary did not start with --version. Run it by hand to see why." }

    # Is the folder on the user's PATH?
    $normalized = $InstallDir.TrimEnd('\')
    $onPath = $false
    foreach ($entry in ($env:PATH -split ';')) {
        if ($entry.TrimEnd('\') -ieq $normalized) { $onPath = $true; break }
    }
    Write-Host ''
    if ($onPath) {
        Write-Host "Done. Next: run 'agentsflow doctor' to check that git, claude and codex are found."
    } else {
        Write-Host "$InstallDir is not on your PATH. To use 'agentsflow' by name, add it to your user PATH."
        Write-Host 'Run this once in PowerShell, then open a new terminal:'
        Write-Host ''
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';$InstallDir', 'User')"
        Write-Host ''
        Write-Host 'This script did not change your PATH or any other setting.'
        Write-Host "Then run 'agentsflow doctor' to check that git, claude and codex are found."
    }
} finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
