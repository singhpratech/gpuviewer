# gpuviewer one-line installer — Windows (x86_64).
#
#   irm https://raw.githubusercontent.com/singhpratech/gpuviewer/main/install.ps1 | iex
#
# What it does, in order: resolve the latest GitHub release (or -Version /
# $env:GPUVIEWER_VERSION) -> download the zip AND its SHA256SUMS file -> verify the
# checksum -> extract gpuviewer.exe into %LOCALAPPDATA%\Programs\gpuviewer -> add that
# directory to the USER Path (idempotent; never touches the machine Path). No admin
# rights required, nothing else is changed.
#
# SmartScreen note: the exe is not code-signed. Running it from a terminal does not
# trip SmartScreen the way double-clicking does; if Windows still flags it, choose
# "More info -> Run anyway". Integrity is covered the verifiable way instead: the
# SHA256 check below, plus GitHub build provenance on every artifact
# (gh attestation verify <file> -R singhpratech/gpuviewer).

[CmdletBinding()]
param(
    # Install this tag (e.g. v0.1.0) instead of the latest release.
    [string]$Version = $env:GPUVIEWER_VERSION,
    # Install directory (default: %LOCALAPPDATA%\Programs\gpuviewer).
    [string]$BinDir = $env:GPUVIEWER_BIN_DIR
)

$ErrorActionPreference = 'Stop'
$Repo   = 'singhpratech/gpuviewer'
$Target = 'x86_64-pc-windows-msvc'

# Windows PowerShell 5.1 defaults to TLS 1.0 on older boxes; GitHub requires 1.2+.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# PROCESSOR_ARCHITECTURE reflects the *host process* arch: a 32-bit PowerShell on 64-bit
# Windows reports 'x86'. PROCESSOR_ARCHITEW6432 is set only in that WOW64 case and carries
# the true machine arch, so prefer it when present — otherwise an x64 box is wrongly rejected.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64') {
    # x64 emulation on ARM64 Windows may work, but GPU monitoring against an ARM64
    # driver stack is unvalidated — be honest rather than silently install.
    throw "no prebuilt Windows $arch binary (x86_64 only) - build from source: cargo install --locked --git https://github.com/$Repo gpuviewer-tui"
}

# --- resolve version -----------------------------------------------------------------
$buildHint = "Build from source instead: cargo install --locked --git https://github.com/$Repo gpuviewer-tui"
if (-not $Version) {
    try {
        $Version = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest").tag_name
    } catch {
        # Tell 404 (no release yet) apart from 403 (rate limit) and transport failure, so a
        # rate-limited or offline user isn't told to build from source. PS 5.1 and 7 expose
        # the status differently; probe both.
        $code = $null
        if ($_.Exception.Response) {
            try { $code = [int]$_.Exception.Response.StatusCode } catch { $code = $null }
        }
        switch ($code) {
            404 { throw "no published release found yet - gpuviewer binaries appear with the first tagged release. $buildHint" }
            403 { throw "GitHub API rate limit hit (HTTP 403). Wait a bit and retry, or pin a known tag: irm ... | iex; or .\install.ps1 -Version v0.1.0" }
            default {
                if ($code) { throw "unexpected GitHub API response (HTTP $code). $buildHint" }
                else { throw "could not reach the GitHub API ($($_.Exception.Message)). Check connectivity, or pin a tag with -Version." }
            }
        }
    }
}
$plain = $Version -replace '^v', ''
Write-Host "installing gpuviewer $Version ($Target)"

# --- download + verify ----------------------------------------------------------------
$asset = "gpuviewer-$plain-$Target.zip"
$sums  = "SHA256SUMS-$Target"
$base  = "https://github.com/$Repo/releases/download/$Version"
$work  = Join-Path ([IO.Path]::GetTempPath()) "gpuviewer-install-$PID"
New-Item -ItemType Directory -Force -Path $work | Out-Null

try {
    Write-Host "downloading $asset ..."
    try {
        Invoke-WebRequest "$base/$asset" -OutFile (Join-Path $work $asset)
        Invoke-WebRequest "$base/$sums"  -OutFile (Join-Path $work $sums)
    } catch {
        throw "download failed from $base ($($_.Exception.Message)) - is $Version a real release tag? $buildHint"
    }

    Write-Host "verifying checksum ..."
    $expected = (Get-Content (Join-Path $work $sums)) |
        Where-Object { $_ -match [regex]::Escape($asset) } |
        ForEach-Object { ($_ -split '\s+')[0].ToLower() } |
        Select-Object -First 1
    if (-not $expected) { throw "checksum file does not list $asset - refusing to install" }
    $actual = (Get-FileHash (Join-Path $work $asset) -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        throw "SHA256 verification FAILED (expected $expected, got $actual) - refusing to install"
    }

    # --- install ----------------------------------------------------------------------
    if (-not $BinDir) {
        # LOCALAPPDATA is normally always set, but SYSTEM/service accounts and some CI hosts
        # leave it empty — Join-Path would then throw an opaque null-binding error.
        if (-not $env:LOCALAPPDATA) {
            throw "LOCALAPPDATA is not set; cannot pick a default install dir. Pass one with -BinDir or `$env:GPUVIEWER_BIN_DIR."
        }
        $BinDir = Join-Path $env:LOCALAPPDATA 'Programs\gpuviewer'
    }
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Expand-Archive -Path (Join-Path $work $asset) -DestinationPath $work -Force
    $exe = Join-Path $work 'gpuviewer.exe'
    if (-not (Test-Path $exe)) { throw "archive did not contain the expected binary (gpuviewer.exe)" }
    Copy-Item $exe (Join-Path $BinDir 'gpuviewer.exe') -Force
    Write-Host "installed: $(Join-Path $BinDir 'gpuviewer.exe')"

    # --- user PATH (idempotent, user scope only) ---------------------------------------
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $BinDir) {
        # On a fresh account the user Path can be empty; avoid writing a leading ';'.
        $newPath = if ($userPath) { "$userPath;$BinDir" } else { $BinDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $env:Path = "$env:Path;$BinDir"   # current session too
        Write-Host "added $BinDir to your user Path (new terminals pick it up automatically)"
    }

    Write-Host ''
    Write-Host 'run: gpuviewer            (live TUI - already recording)'
    Write-Host '     gpuviewer demo       (8h simulated incident, opens at the throttle onset)'
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
