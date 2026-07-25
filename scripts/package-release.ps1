# Package the Windows Henosis and Crucible executables into a reproducible native archive.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$HenosisBinaryPath,
    [Parameter(Mandatory = $true)][string]$CrucibleBinaryPath,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$Target,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [Parameter(Mandatory = $true)][long]$SourceDateEpoch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Stop packaging with a release-specific validation error.
function Stop-ReleasePackaging {
    param([Parameter(Mandatory = $true)][string]$Message)
    throw "package-release: $Message"
}

# Reject archive components that could create an ambiguous archive name.
function Assert-ReleaseComponent {
    param([Parameter(Mandatory = $true)][string]$Name, [Parameter(Mandatory = $true)][string]$Value)
    if ($Value -notmatch '^[A-Za-z0-9._-]+$') { Stop-ReleasePackaging "$Name contains unsupported characters" }
}

# Add one staged file to a ZIP using a normalized timestamp.
function Add-ReleaseEntry {
    param(
        [Parameter(Mandatory = $true)][System.IO.Compression.ZipArchive]$Archive,
        [Parameter(Mandatory = $true)][string]$EntryName,
        [Parameter(Mandatory = $true)][string]$SourcePath,
        [Parameter(Mandatory = $true)][DateTimeOffset]$Timestamp
    )
    $entry = $Archive.CreateEntry($EntryName, [System.IO.Compression.CompressionLevel]::Optimal)
    $entry.LastWriteTime = $Timestamp
    $input = [System.IO.File]::OpenRead($SourcePath)
    try { $output = $entry.Open(); try { $input.CopyTo($output) } finally { $output.Dispose() } } finally { $input.Dispose() }
}

# Write the release-local installation instructions.
function Write-InstallReadme {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$ReleaseVersion, [Parameter(Mandatory = $true)][string]$ReleaseTarget)
    @"
# Henosis $ReleaseVersion

This archive contains the native ``henosis.exe`` runtime and ``crucible.exe`` quality gate for ``$ReleaseTarget``.

Verify this archive with the release ``SHA256SUMS`` manifest before installing.
Run ``.\\install.ps1 -Headless`` from the extracted archive for a per-user offline installation.
The archive marker binds the installer to both adjacent verified executables.
The installer activates both programs, runs ``henosis init --quick``, and rolls back both if initialization fails.
"@ | Set-Content -LiteralPath $Path -NoNewline -Encoding utf8
}

if (-not (Test-Path -LiteralPath $HenosisBinaryPath -PathType Leaf)) { Stop-ReleasePackaging "Henosis binary does not exist: $HenosisBinaryPath" }
if (-not (Test-Path -LiteralPath $CrucibleBinaryPath -PathType Leaf)) { Stop-ReleasePackaging "Crucible binary does not exist: $CrucibleBinaryPath" }
if ($SourceDateEpoch -lt 315532800) { Stop-ReleasePackaging 'SOURCE_DATE_EPOCH must not predate 1980-01-01 for ZIP compatibility' }
Assert-ReleaseComponent -Name version -Value $Version
Assert-ReleaseComponent -Name target -Value $Target
$repositoryDirectory = Split-Path -Parent $PSScriptRoot
$installer = Join-Path $repositoryDirectory 'install.ps1'
foreach ($required in @((Join-Path $repositoryDirectory 'LICENSE'), $installer)) { if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { Stop-ReleasePackaging "required file is missing: $required" } }
$root = "henosis-$Version-$Target"
$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("henosis-package-" + [guid]::NewGuid().ToString('N'))
$archivePath = Join-Path $OutputDirectory "$root.zip"
try {
    $content = Join-Path $stage $root
    New-Item -ItemType Directory -Force -Path $content, $OutputDirectory | Out-Null
    Copy-Item -LiteralPath $HenosisBinaryPath -Destination (Join-Path $content 'henosis.exe') -Force
    Copy-Item -LiteralPath $CrucibleBinaryPath -Destination (Join-Path $content 'crucible.exe') -Force
    Copy-Item -LiteralPath $installer -Destination (Join-Path $content 'install.ps1') -Force
    Copy-Item -LiteralPath (Join-Path $repositoryDirectory 'LICENSE') -Destination (Join-Path $content 'LICENSE') -Force
    "v$Version $Target" | Set-Content -LiteralPath (Join-Path $content 'HENOSIS_ARCHIVE') -NoNewline -Encoding ascii
    Write-InstallReadme -Path (Join-Path $content 'README.md') -ReleaseVersion $Version -ReleaseTarget $Target
    $timestamp = [DateTimeOffset]::FromUnixTimeSeconds($SourceDateEpoch)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $stream = [System.IO.File]::Open($archivePath, [System.IO.FileMode]::Create)
    try {
        $archive = [System.IO.Compression.ZipArchive]::new($stream, [System.IO.Compression.ZipArchiveMode]::Create)
        try { foreach ($name in @('HENOSIS_ARCHIVE', 'LICENSE', 'README.md', 'crucible.exe', 'henosis.exe', 'install.ps1')) { Add-ReleaseEntry -Archive $archive -EntryName "$root/$name" -SourcePath (Join-Path $content $name) -Timestamp $timestamp } } finally { $archive.Dispose() }
    } finally { $stream.Dispose() }
} finally { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }
Write-Output $archivePath
