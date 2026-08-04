# Install the checksum-pinned Windows protoc binary used by GitHub Actions.

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# Keep the compiler version and official release digest coupled in one place.
$protocVersion = "35.1"
$expectedArchiveSha256 = "5d3ff218d7d91eea95f7569bcb5a98f3030f8996d44151279d9772edcff76082"

if ([string]::IsNullOrWhiteSpace($env:RUNNER_TEMP)) {
    throw "RUNNER_TEMP must identify the isolated GitHub Actions workspace."
}
if ([string]::IsNullOrWhiteSpace($env:GITHUB_PATH)) {
    throw "GITHUB_PATH must be available to export protoc for later steps."
}

$archiveName = "protoc-$protocVersion-win64.zip"
$archivePath = Join-Path $env:RUNNER_TEMP $archiveName
$installRoot = Join-Path $env:RUNNER_TEMP "protoc-$protocVersion"
$downloadUrl = "https://github.com/protocolbuffers/protobuf/releases/download/v$protocVersion/$archiveName"

Invoke-WebRequest -Uri $downloadUrl -OutFile $archivePath
$actualArchiveSha256 = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
if (-not [string]::Equals(
    $actualArchiveSha256,
    $expectedArchiveSha256,
    [System.StringComparison]::OrdinalIgnoreCase
)) {
    throw "Downloaded protoc archive failed SHA-256 verification."
}

Expand-Archive -LiteralPath $archivePath -DestinationPath $installRoot -Force
$protocBin = Join-Path $installRoot "bin"
$protocExecutable = Join-Path $protocBin "protoc.exe"
if (-not (Test-Path -LiteralPath $protocExecutable -PathType Leaf)) {
    throw "Verified protoc archive did not contain bin/protoc.exe."
}

$reportedVersion = (& $protocExecutable --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Verified protoc executable failed its version check."
}
Write-Host "Installed $reportedVersion from the checksum-verified official release."
$protocBin | Out-File -FilePath $env:GITHUB_PATH -Encoding utf8 -Append
