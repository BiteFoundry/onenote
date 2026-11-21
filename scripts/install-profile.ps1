<#
.SYNOPSIS
    Installs the `onenote` terminal launcher on Windows.

.DESCRIPTION
    1. Ensures <RepoRoot>\target\release\onenote.exe exists (downloads the prebuilt
       release binary from GitHub `BiteFoundry/onenote` via `gh` when missing, so
       Rust/Cargo is NOT required on target PCs).
    2. Registers/updates `# >>> onenote launcher >>>` in the PowerShell profile.
    3. If $HOME\Tools\bin exists, writes `onenote.cmd` and removes any matching launcher from the previous install.

.EXAMPLE
    pwsh -File .\scripts\install-profile.ps1
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$ProfilePath = $PROFILE,
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($ProfilePath)) {
    throw 'ProfilePath is empty. Pass -ProfilePath explicitly or run inside a PowerShell host where $PROFILE is defined.'
}

$resolvedRoot = if (Test-Path -LiteralPath $RepoRoot) {
    (Resolve-Path -LiteralPath $RepoRoot).Path
} else {
    $RepoRoot
}

$releaseDir = Join-Path $resolvedRoot 'target\release'
$releaseExe = Join-Path $releaseDir 'onenote.exe'

if (-not (Test-Path -LiteralPath $releaseExe)) {
    $gh = Get-Command gh -ErrorAction SilentlyContinue
    if ($gh) {
        Write-Output "[onenote] Downloading prebuilt onenote.exe from BiteFoundry/onenote GitHub Release..."
        New-Item -ItemType Directory -Force -Path $releaseDir | Out-Null
        & $gh.Source release download -R 'BiteFoundry/onenote' --pattern 'onenote.exe' --dir $releaseDir --clobber
    }
}

$escapedRoot = $resolvedRoot.Replace("'", "''")
$startMarker = '# >>> onenote launcher >>>'
$endMarker = '# <<< onenote launcher <<<'

$block = @"
$startMarker
# Opens the Microsoft OneNote terminal client (CLI/TUI/HTTP/MCP) from anywhere.
function onenote {
    `$root = '$escapedRoot'
    `$exe = Join-Path `$root 'target\release\onenote.exe'
    if (-not (Test-Path -LiteralPath `$exe)) {
        if (Get-Command gh -ErrorAction SilentlyContinue) {
            `$relDir = Split-Path -Parent `$exe
            New-Item -ItemType Directory -Force -Path `$relDir | Out-Null
            gh release download -R 'BiteFoundry/onenote' --pattern 'onenote.exe' --dir `$relDir --clobber 2>`$null
        }
    }
    if (-not (Test-Path -LiteralPath `$exe)) {
        cargo build --release --manifest-path (Join-Path `$root 'Cargo.toml')
        if (`$LASTEXITCODE -ne 0) { return }
    }
    & `$exe @args
}
$endMarker
"@.TrimEnd("`r", "`n")

$parentDir = Split-Path -Parent $ProfilePath
if ($parentDir -and -not (Test-Path -LiteralPath $parentDir)) {
    if ($PSCmdlet.ShouldProcess($parentDir, 'Create PowerShell profile parent directory')) {
        New-Item -ItemType Directory -Force -Path $parentDir | Out-Null
    }
}

$existing = ''
if (Test-Path -LiteralPath $ProfilePath) {
    $existing = Get-Content -LiteralPath $ProfilePath -Raw -Encoding UTF8
    if ($null -eq $existing) { $existing = '' }
}

$pattern = '(?ms)^# >>> onenote launcher >>>\r?\n.*?^# <<< onenote launcher <<<\r?\n?'
if ([regex]::IsMatch($existing, $pattern)) {
    $updated = [regex]::Replace($existing, $pattern, { param($m) $block + [Environment]::NewLine }, 1)
    $action = 'Update onenote launcher block'
} elseif ([string]::IsNullOrWhiteSpace($existing)) {
    $updated = $block + [Environment]::NewLine
    $action = 'Create profile with onenote launcher block'
} else {
    $trimmed = $existing.TrimEnd("`r", "`n")
    $nl = [Environment]::NewLine
    $updated = $trimmed + $nl + $nl + $block + $nl
    $action = 'Append onenote launcher block'
}

if ($updated -ne $existing) {
    if ($PSCmdlet.ShouldProcess($ProfilePath, $action)) {
        $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllText($ProfilePath, $updated, $utf8NoBom)
        Write-Output "$action in $ProfilePath."
    }
} else {
    Write-Output "onenote launcher in $ProfilePath is already up to date."
}

$toolsBin = Join-Path $HOME 'Tools\bin'
if (Test-Path -LiteralPath $toolsBin) {
    $shimPath = Join-Path $toolsBin 'onenote.cmd'
    $shimContent = "@echo off`r`nchcp 65001 >nul`r`n`"$releaseExe`" %*`r`n"
    if ($PSCmdlet.ShouldProcess($shimPath, 'Write PATH shim onenote.cmd')) {
        [System.IO.File]::WriteAllText($shimPath, $shimContent, [System.Text.Encoding]::ASCII)
        Write-Output "Updated PATH shim: $shimPath"
    }

    $legacyShim = Join-Path $toolsBin 'note.cmd'
    if ((Test-Path -LiteralPath $legacyShim -PathType Leaf) -and
        ([System.IO.File]::ReadAllText($legacyShim) -ceq $shimContent) -and
        $PSCmdlet.ShouldProcess($legacyShim, 'Remove previous OneNote PATH shim')) {
        Remove-Item -LiteralPath $legacyShim
    }
}
