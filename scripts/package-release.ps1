# Package one Windows Henosis server binary into a reproducible release archive.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryPath,
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$Target,
    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory,
    [Parameter(Mandatory = $true)]
    [long]$SourceDateEpoch
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
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Value
    )

    if ($Value -notmatch '^[A-Za-z0-9._-]+$') {
        Stop-ReleasePackaging "$Name contains unsupported characters"
    }
}

# Write the release-local installation guide that accompanies the native Windows binary.
function Write-InstallReadme {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ReleaseVersion,
        [Parameter(Mandatory = $true)][string]$ReleaseTarget
    )

    $contents = @'
# Henosis %%VERSION%%

This archive contains the native Henosis server binary for `%%TARGET%%`.

## Verify and install

Before installation, verify this archive against the `SHA256SUMS` file and
its GitHub artifact attestation on the matching release. The attestation is
signed with the GitHub Actions OIDC identity for Syntheos-Systems/henosis.

Place `syntheos-server.exe` in a directory on your PATH, then run it in a
configured deployment. The bundled `install.sh` provides a configured local
service install on a supported Unix host. After that install passes its health
check, `demo-governed-mission.sh` requires `curl` and Python 3 and proves
authorized execution, hostile-input denial, and correlated audit projection.
See the repository README and SECURITY.md before exposing a deployment.
'@
    $contents.Replace('%%VERSION%%', $ReleaseVersion).Replace('%%TARGET%%', $ReleaseTarget) |
        Set-Content -LiteralPath $Path -NoNewline -Encoding utf8
}

# Add one staged file to the ZIP with normalized metadata and deterministic compression order.
function Add-ReleaseArchiveEntry {
    param(
        [Parameter(Mandatory = $true)][System.IO.Compression.ZipArchive]$Archive,
        [Parameter(Mandatory = $true)][string]$EntryName,
        [Parameter(Mandatory = $true)][string]$SourcePath,
        [Parameter(Mandatory = $true)][DateTimeOffset]$Timestamp
    )

    $entry = $Archive.CreateEntry($EntryName, [System.IO.Compression.CompressionLevel]::Optimal)
    $entry.LastWriteTime = $Timestamp
    $source = [System.IO.File]::OpenRead($SourcePath)
    try {
        $destination = $entry.Open()
        try {
            $source.CopyTo($destination)
        }
        finally {
            $destination.Dispose()
        }
    }
    finally {
        $source.Dispose()
    }
}

if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    Stop-ReleasePackaging "binary does not exist: $BinaryPath"
}
if ($SourceDateEpoch -lt 315532800) {
    Stop-ReleasePackaging 'SOURCE_DATE_EPOCH must not predate 1980-01-01 for ZIP compatibility'
}

Assert-ReleaseComponent -Name 'version' -Value $Version
Assert-ReleaseComponent -Name 'target' -Value $Target

$repositoryDirectory = Split-Path -Parent $PSScriptRoot
$licensePath = Join-Path $repositoryDirectory 'LICENSE'
if (-not (Test-Path -LiteralPath $licensePath -PathType Leaf)) {
    Stop-ReleasePackaging 'repository LICENSE is missing'
}
$installerPath = Join-Path $repositoryDirectory 'install.sh'
if (-not (Test-Path -LiteralPath $installerPath -PathType Leaf)) {
    Stop-ReleasePackaging 'repository installer is missing'
}
$missionPath = Join-Path $repositoryDirectory 'scripts/demo-governed-mission.sh'
if (-not (Test-Path -LiteralPath $missionPath -PathType Leaf)) {
    Stop-ReleasePackaging 'governed mission script is missing'
}

$archiveRoot = "henosis-$Version-$Target"
$archivePath = Join-Path $OutputDirectory "$archiveRoot.zip"
$stagingDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("henosis-release-" + [Guid]::NewGuid().ToString('N'))
$archiveTimestamp = [DateTimeOffset]::FromUnixTimeSeconds($SourceDateEpoch)

try {
    New-Item -ItemType Directory -Path $stagingDirectory -Force | Out-Null
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    $readmePath = Join-Path $stagingDirectory 'README.md'
    Copy-Item -LiteralPath $BinaryPath -Destination (Join-Path $stagingDirectory 'syntheos-server.exe')
    Copy-Item -LiteralPath $licensePath -Destination (Join-Path $stagingDirectory 'LICENSE')
    Copy-Item -LiteralPath $installerPath -Destination (Join-Path $stagingDirectory 'install.sh')
    Copy-Item -LiteralPath $missionPath -Destination (Join-Path $stagingDirectory 'demo-governed-mission.sh')
    Write-InstallReadme -Path $readmePath -ReleaseVersion $Version -ReleaseTarget $Target

    Add-Type -AssemblyName System.IO.Compression
    $stream = [System.IO.File]::Open($archivePath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
    try {
        $archive = [System.IO.Compression.ZipArchive]::new($stream, [System.IO.Compression.ZipArchiveMode]::Create, $false)
        try {
            Add-ReleaseArchiveEntry -Archive $archive -EntryName "$archiveRoot/syntheos-server.exe" -SourcePath (Join-Path $stagingDirectory 'syntheos-server.exe') -Timestamp $archiveTimestamp
            Add-ReleaseArchiveEntry -Archive $archive -EntryName "$archiveRoot/LICENSE" -SourcePath (Join-Path $stagingDirectory 'LICENSE') -Timestamp $archiveTimestamp
            Add-ReleaseArchiveEntry -Archive $archive -EntryName "$archiveRoot/demo-governed-mission.sh" -SourcePath (Join-Path $stagingDirectory 'demo-governed-mission.sh') -Timestamp $archiveTimestamp
            Add-ReleaseArchiveEntry -Archive $archive -EntryName "$archiveRoot/install.sh" -SourcePath (Join-Path $stagingDirectory 'install.sh') -Timestamp $archiveTimestamp
            Add-ReleaseArchiveEntry -Archive $archive -EntryName "$archiveRoot/README.md" -SourcePath $readmePath -Timestamp $archiveTimestamp
        }
        finally {
            $archive.Dispose()
        }
    }
    finally {
        $stream.Dispose()
    }
}
finally {
    if (Test-Path -LiteralPath $stagingDirectory) {
        Remove-Item -LiteralPath $stagingDirectory -Recurse -Force
    }
}

Write-Output $archivePath
