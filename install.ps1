# Install a verified native Henosis release for the current Windows platform.

[CmdletBinding()]
param(
    [string]$Version = $(if ($env:HENOSIS_VERSION) { $env:HENOSIS_VERSION } else { 'v0.1.0-alpha.1' }),
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

# Return the release target supported by this Windows host.
function Get-ReleaseTarget {
    if (-not $IsWindows) { Stop-Install 'this installer supports Windows only' }
    if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne [System.Runtime.InteropServices.Architecture]::X64) {
        Stop-Install "unsupported Windows architecture: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)"
    }
    return 'x86_64-pc-windows-msvc'
}

$releaseBase = Get-ReleaseBase

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

# Install the archive transactionally and initialize the binary before committing it.
function Install-Release {
    $target = Get-ReleaseTarget
    $archiveName = "henosis-$($Version.TrimStart('v'))-$target.zip"
    $workDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("henosis-install-" + [guid]::NewGuid().ToString('N'))
    $destination = Join-Path $InstallDirectory 'henosis.exe'
    $backup = Join-Path $workDirectory 'henosis.previous.exe'
    $activated = $false
    try {
        New-Item -ItemType Directory -Force -Path $workDirectory, $InstallDirectory | Out-Null
        Get-ReleaseFile -Uri "$releaseBase/$Version/SHA256SUMS" -OutFile (Join-Path $workDirectory 'SHA256SUMS')
        $archivePath = Join-Path $workDirectory $archiveName
        Get-ReleaseFile -Uri "$releaseBase/$Version/$archiveName" -OutFile $archivePath
        Assert-ArchiveChecksum -ManifestPath (Join-Path $workDirectory 'SHA256SUMS') -ArchivePath $archivePath -ArchiveName $archiveName
        Expand-Archive -LiteralPath $archivePath -DestinationPath $workDirectory -Force
        $candidate = Join-Path $workDirectory "henosis-$($Version.TrimStart('v'))-$target\\henosis.exe"
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) { Stop-Install 'verified archive does not contain henosis.exe' }
        if (Test-Path -LiteralPath $destination -PathType Leaf) { Copy-Item -LiteralPath $destination -Destination $backup -Force }
        Copy-Item -LiteralPath $candidate -Destination $destination -Force
        $activated = $true
        & $destination init --quick
        if ($LASTEXITCODE -ne 0) { Stop-Install 'henosis init --quick failed; restored the previous installation' }
        $activated = $false
        if ($Headless) {
            [pscustomobject]@{ ok = $true; binary = $destination; version = $Version; target = $target } | ConvertTo-Json -Compress
        }
        else { Write-Host "henosis-installer: installed $destination" }
    }
    finally {
        if ($activated) {
            if (Test-Path -LiteralPath $backup -PathType Leaf) { Move-Item -LiteralPath $backup -Destination $destination -Force }
            else { Remove-Item -LiteralPath $destination -Force -ErrorAction SilentlyContinue }
        }
        Remove-Item -LiteralPath $workDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Install-Release
