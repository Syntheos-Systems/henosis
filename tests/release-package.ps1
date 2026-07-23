# Validate reproducible Windows release packaging by executing the PowerShell packager.

[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repositoryDirectory = Split-Path -Parent $PSScriptRoot
$testDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("henosis-release-test-" + [Guid]::NewGuid().ToString('N'))
$version = '0.1.0'
$target = 'x86_64-pc-windows-msvc'
$archiveRoot = "henosis-$version-$target"
$sourceDateEpoch = 1784768092L

# Stop the contract test with a precise assertion failure.
function Assert-Contract {
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if (-not $Condition) {
        throw "release-package-test: $Message"
    }
}

# Return the sorted entry names and validate normalized timestamps in one ZIP archive.
function Test-ArchiveContract {
    param(
        [Parameter(Mandatory = $true)][string]$ArchivePath,
        [Parameter(Mandatory = $true)][string]$FixtureBinary
    )

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        $expectedTimestamp = [DateTimeOffset]::FromUnixTimeSeconds($sourceDateEpoch)
        $expectedEntries = @(
            "$archiveRoot/LICENSE"
            "$archiveRoot/README.md"
            "$archiveRoot/demo-governed-mission.sh"
            "$archiveRoot/install.sh"
            "$archiveRoot/syntheos-server.exe"
        ) | Sort-Object
        $actualEntries = @($archive.Entries | ForEach-Object { $_.FullName } | Sort-Object)
        Assert-Contract -Condition ($null -eq (Compare-Object $expectedEntries $actualEntries)) -Message 'ZIP members differ from the release contract'

        foreach ($entry in $archive.Entries) {
            Assert-Contract -Condition ($entry.LastWriteTime.ToUnixTimeSeconds() -eq $expectedTimestamp.ToUnixTimeSeconds()) -Message "ZIP timestamp is not normalized: $($entry.FullName)"
        }

        $readmeEntry = $archive.GetEntry("$archiveRoot/README.md")
        Assert-Contract -Condition ($null -ne $readmeEntry) -Message 'release README is missing'
        $reader = [System.IO.StreamReader]::new($readmeEntry.Open())
        try {
            $readme = $reader.ReadToEnd()
        }
        finally {
            $reader.Dispose()
        }
        Assert-Contract -Condition ($readme.Contains('GitHub artifact attestation')) -Message 'release README does not document artifact verification'
        Assert-Contract -Condition ($readme.Contains('Python 3')) -Message 'release README does not document the mission runtime'

        $binaryEntry = $archive.GetEntry("$archiveRoot/syntheos-server.exe")
        Assert-Contract -Condition ($null -ne $binaryEntry) -Message 'Windows binary is missing'
        $entryStream = $binaryEntry.Open()
        $fixtureStream = [System.IO.File]::OpenRead($FixtureBinary)
        try {
            $entryHash = [System.Security.Cryptography.SHA256]::HashData($entryStream)
            $fixtureHash = [System.Security.Cryptography.SHA256]::HashData($fixtureStream)
        }
        finally {
            $entryStream.Dispose()
            $fixtureStream.Dispose()
        }
        Assert-Contract -Condition ([Convert]::ToHexString($entryHash) -eq [Convert]::ToHexString($fixtureHash)) -Message 'packaged Windows binary differs from its input'
    }
    finally {
        $archive.Dispose()
    }
}

try {
    New-Item -ItemType Directory -Path $testDirectory -Force | Out-Null
    $binaryPath = Join-Path $testDirectory 'syntheos-server.exe'
    [System.IO.File]::WriteAllBytes($binaryPath, [byte[]](0x4d, 0x5a, 0x90, 0x00))
    $firstOutput = Join-Path $testDirectory 'first'
    $secondOutput = Join-Path $testDirectory 'second'

    & "$repositoryDirectory/scripts/package-release.ps1" -BinaryPath $binaryPath -Version $version -Target $target -OutputDirectory $firstOutput -SourceDateEpoch $sourceDateEpoch | Out-Null
    & "$repositoryDirectory/scripts/package-release.ps1" -BinaryPath $binaryPath -Version $version -Target $target -OutputDirectory $secondOutput -SourceDateEpoch $sourceDateEpoch | Out-Null

    $archiveName = "$archiveRoot.zip"
    $firstArchive = Join-Path $firstOutput $archiveName
    $secondArchive = Join-Path $secondOutput $archiveName
    Assert-Contract -Condition (Test-Path -LiteralPath $firstArchive -PathType Leaf) -Message 'first ZIP archive was not created'
    Assert-Contract -Condition (Test-Path -LiteralPath $secondArchive -PathType Leaf) -Message 'second ZIP archive was not created'

    $firstHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $firstArchive).Hash
    $secondHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $secondArchive).Hash
    Assert-Contract -Condition ($firstHash -eq $secondHash) -Message 'Windows release ZIP is not reproducible'
    Test-ArchiveContract -ArchivePath $firstArchive -FixtureBinary $binaryPath

    $missingFailed = $false
    try {
        & "$repositoryDirectory/scripts/package-release.ps1" -BinaryPath (Join-Path $testDirectory 'missing.exe') -Version $version -Target $target -OutputDirectory (Join-Path $testDirectory 'invalid') -SourceDateEpoch $sourceDateEpoch | Out-Null
    }
    catch {
        $missingFailed = $true
    }
    Assert-Contract -Condition $missingFailed -Message 'packager accepted a missing binary'

    Write-Output 'Windows release package contract passed'
}
finally {
    if (Test-Path -LiteralPath $testDirectory) {
        Remove-Item -LiteralPath $testDirectory -Recurse -Force
    }
}
