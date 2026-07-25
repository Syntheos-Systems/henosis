# Install a verified native Henosis release for the current Windows platform.

[CmdletBinding()]
param(
    [string]$Version = $(if ($env:HENOSIS_VERSION) { $env:HENOSIS_VERSION } else { 'v0.1.0-alpha.6' }),
    [string]$InstallDirectory = $(if ($env:HENOSIS_INSTALL_DIR) { $env:HENOSIS_INSTALL_DIR } else { Join-Path $HOME '.local\\bin' }),
    [switch]$Headless
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Write either a human error or a machine-readable headless error and stop.
function Stop-Install {
    param([Parameter(Mandatory = $true)][string]$Message)

    if ($Headless) {
        [pscustomobject]@{ ok = $false; error = $Message } | ConvertTo-Json -Compress
    }
    else {
        Write-Error "henosis-installer: $Message"
    }
    exit 1
}

# Return an absolute HTTPS release base without credentials, query, or fragment.
function Get-ReleaseBase {
    $candidate = if ($env:HENOSIS_RELEASE_BASE) { $env:HENOSIS_RELEASE_BASE.TrimEnd('/') } else { 'https://github.com/Syntheos-Systems/henosis/releases/download' }
    $uri = $null
    if (-not [Uri]::TryCreate($candidate, [UriKind]::Absolute, [ref]$uri) -or
        $uri.Scheme -ne [Uri]::UriSchemeHttps -or
        -not [string]::IsNullOrEmpty($uri.UserInfo) -or
        -not [string]::IsNullOrEmpty($uri.Query) -or
        -not [string]::IsNullOrEmpty($uri.Fragment)) {
        Stop-Install 'HENOSIS_RELEASE_BASE must be an absolute HTTPS URL without credentials, query, or fragment'
    }
    return $uri.AbsoluteUri.TrimEnd('/')
}

# Return an absolute HTTPS release metadata API without credentials, query, or fragment.
function Get-ReleaseApi {
    $candidate = if ($env:HENOSIS_RELEASE_API) { $env:HENOSIS_RELEASE_API.TrimEnd('/') } else { 'https://api.github.com/repos/Syntheos-Systems/henosis/releases/tags' }
    $uri = $null
    if (-not [Uri]::TryCreate($candidate, [UriKind]::Absolute, [ref]$uri) -or
        $uri.Scheme -ne [Uri]::UriSchemeHttps -or
        -not [string]::IsNullOrEmpty($uri.UserInfo) -or
        -not [string]::IsNullOrEmpty($uri.Query) -or
        -not [string]::IsNullOrEmpty($uri.Fragment)) {
        Stop-Install 'HENOSIS_RELEASE_API must be an absolute HTTPS URL without credentials, query, or fragment'
    }
    return $uri.AbsoluteUri.TrimEnd('/')
}

# Return the release target supported by this Windows host.
function Get-ReleaseTarget {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        Stop-Install 'this installer supports Windows only'
    }
    if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne [System.Runtime.InteropServices.Architecture]::X64) {
        Stop-Install "unsupported Windows architecture: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)"
    }
    return 'x86_64-pc-windows-msvc'
}

# Return the SHA-256 digest for one file in lowercase hexadecimal form.
function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string]$Path)

    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

# Extract the mandatory checksum for one archive and verify its contents.
function Assert-ArchiveChecksum {
    param(
        [Parameter(Mandatory = $true)][string]$ManifestPath,
        [Parameter(Mandatory = $true)][string]$ArchivePath,
        [Parameter(Mandatory = $true)][string]$ArchiveName
    )

    $entries = @(Get-Content -LiteralPath $ManifestPath | Where-Object { $_ -match "^([0-9a-fA-F]{64})\\s+\\*?$([regex]::Escape($ArchiveName))$" })
    if ($entries.Count -ne 1) { Stop-Install "checksum manifest has no unique entry for $ArchiveName" }
    $expected = ([regex]::Match($entries[0], '^[0-9a-fA-F]{64}')).Value.ToLowerInvariant()
    if ((Get-Sha256 -Path $ArchivePath) -ne $expected) { Stop-Install "checksum verification failed for $ArchiveName" }
}

# Download a release file with PowerShell's native HTTPS client.
function Get-ReleaseFile {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$OutFile
    )

    Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $OutFile
}

# Require the release API document to identify a published immutable release.
function Assert-ImmutableRelease {
    param([Parameter(Mandatory = $true)][string]$Path)

    if ((Get-Item -LiteralPath $Path).Length -gt 1MB) { Stop-Install 'release metadata exceeds the 1 MiB safety limit' }
    try { $metadata = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json }
    catch { Stop-Install 'release metadata is not valid JSON' }
    if ($metadata.tag_name -ne $Version) { Stop-Install 'release metadata does not match the selected version' }
    if ($metadata.draft -ne $false) { Stop-Install 'selected release is not published' }
    if ($metadata.immutable -ne $true) { Stop-Install 'selected release is not immutable' }
}

# Resolve adjacent Henosis and Crucible binaries from a verified release archive.
function Get-ArchiveBinaries {
    param([Parameter(Mandatory = $true)][string]$Target)

    $marker = Join-Path $PSScriptRoot 'HENOSIS_ARCHIVE'
    if (-not (Test-Path -LiteralPath $marker -PathType Leaf)) { return $null }
    $parts = @((Get-Content -LiteralPath $marker -Raw).Trim() -split '\s+')
    if ($parts.Count -ne 2 -or $parts[0] -ne $Version -or $parts[1] -ne $Target) {
        Stop-Install 'release archive marker does not match this installer'
    }
    $henosisCandidate = Join-Path $PSScriptRoot 'henosis.exe'
    $crucibleCandidate = Join-Path $PSScriptRoot 'crucible.exe'
    if (-not (Test-Path -LiteralPath $henosisCandidate -PathType Leaf)) {
        Stop-Install 'release archive does not contain an adjacent henosis.exe'
    }
    if (-not (Test-Path -LiteralPath $crucibleCandidate -PathType Leaf)) {
        Stop-Install 'release archive does not contain an adjacent crucible.exe'
    }
    return [pscustomobject]@{ Henosis = $henosisCandidate; Crucible = $crucibleCandidate }
}

# Install the archive transactionally and initialize the binary before committing it.
function Install-Release {
    if ($Version -notmatch '^v[0-9][0-9A-Za-z.-]*$') { Stop-Install 'release version must be a valid v-prefixed tag' }
    $target = Get-ReleaseTarget
    $archiveName = "henosis-$($Version.TrimStart('v'))-$target.zip"
    $workDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("henosis-install-" + [guid]::NewGuid().ToString('N'))
    $destination = Join-Path $InstallDirectory 'henosis.exe'
    $crucibleDestination = Join-Path $InstallDirectory 'crucible.exe'
    $backup = Join-Path $workDirectory 'henosis.previous.exe'
    $crucibleBackup = Join-Path $workDirectory 'crucible.previous.exe'
    $activated = $false
    $crucibleActivated = $false
    try {
        New-Item -ItemType Directory -Force -Path $workDirectory, $InstallDirectory | Out-Null
        $candidates = Get-ArchiveBinaries -Target $target
        if ($null -eq $candidates) {
            $releaseBase = Get-ReleaseBase
            $releaseApi = Get-ReleaseApi
            $metadataPath = Join-Path $workDirectory 'release.json'
            Get-ReleaseFile -Uri "$releaseApi/$Version" -OutFile $metadataPath
            Assert-ImmutableRelease -Path $metadataPath
            Get-ReleaseFile -Uri "$releaseBase/$Version/SHA256SUMS" -OutFile (Join-Path $workDirectory 'SHA256SUMS')
            $archivePath = Join-Path $workDirectory $archiveName
            Get-ReleaseFile -Uri "$releaseBase/$Version/$archiveName" -OutFile $archivePath
            Assert-ArchiveChecksum -ManifestPath (Join-Path $workDirectory 'SHA256SUMS') -ArchivePath $archivePath -ArchiveName $archiveName
            Expand-Archive -LiteralPath $archivePath -DestinationPath $workDirectory -Force
            $contentDirectory = Join-Path $workDirectory "henosis-$($Version.TrimStart('v'))-$target"
            $candidate = Join-Path $contentDirectory 'henosis.exe'
            $crucibleCandidate = Join-Path $contentDirectory 'crucible.exe'
            if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) { Stop-Install 'verified archive does not contain henosis.exe' }
            if (-not (Test-Path -LiteralPath $crucibleCandidate -PathType Leaf)) { Stop-Install 'verified archive does not contain crucible.exe' }
            $candidates = [pscustomobject]@{ Henosis = $candidate; Crucible = $crucibleCandidate }
        }
        if (Test-Path -LiteralPath $destination -PathType Leaf) { Copy-Item -LiteralPath $destination -Destination $backup -Force }
        if (Test-Path -LiteralPath $crucibleDestination -PathType Leaf) { Copy-Item -LiteralPath $crucibleDestination -Destination $crucibleBackup -Force }
        Copy-Item -LiteralPath $candidates.Henosis -Destination $destination -Force
        $activated = $true
        Copy-Item -LiteralPath $candidates.Crucible -Destination $crucibleDestination -Force
        $crucibleActivated = $true
        & $destination init --quick
        if ($LASTEXITCODE -ne 0) { Stop-Install 'henosis init --quick failed; restored the previous Henosis and Crucible installation' }
        $activated = $false
        $crucibleActivated = $false
        if ($Headless) {
            [pscustomobject]@{ ok = $true; binary = $destination; crucible = $crucibleDestination; version = $Version; target = $target } | ConvertTo-Json -Compress
        }
        else { Write-Host "henosis-installer: installed $destination" }
    }
    finally {
        if ($activated) {
            if (Test-Path -LiteralPath $backup -PathType Leaf) { Move-Item -LiteralPath $backup -Destination $destination -Force }
            else { Remove-Item -LiteralPath $destination -Force -ErrorAction SilentlyContinue }
        }
        if ($crucibleActivated) {
            if (Test-Path -LiteralPath $crucibleBackup -PathType Leaf) { Move-Item -LiteralPath $crucibleBackup -Destination $crucibleDestination -Force }
            else { Remove-Item -LiteralPath $crucibleDestination -Force -ErrorAction SilentlyContinue }
        }
        Remove-Item -LiteralPath $workDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Install-Release
