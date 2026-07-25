# Verify the Windows native archive names the public Henosis executable.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("henosis-package-test-" + [guid]::NewGuid().ToString('N'))

# Remove only the temporary Windows package test workspace.
function Clear-TestRoot {
    Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
}

# Stop the Windows package contract with a diagnostic.
function Stop-Test {
    param([Parameter(Mandatory = $true)][string]$Message)
    throw "FAIL: $Message"
}

try {
    New-Item -ItemType Directory -Path $testRoot | Out-Null
    $binary = Join-Path $testRoot 'henosis-fixture.exe'
    $crucible = Join-Path $testRoot 'crucible-fixture.exe'
    [System.IO.File]::WriteAllBytes($binary, [byte[]](77,90,0,0))
    [System.IO.File]::WriteAllBytes($crucible, [byte[]](77,90,0,0))
    $output = Join-Path $testRoot 'dist'
    & (Join-Path $repository 'scripts/package-release.ps1') -HenosisBinaryPath $binary -CrucibleBinaryPath $crucible -Version '0.1.0-alpha.6' -Target 'x86_64-pc-windows-msvc' -OutputDirectory $output -SourceDateEpoch 1784768092 | Out-Null
    $archive = Join-Path $output 'henosis-0.1.0-alpha.6-x86_64-pc-windows-msvc.zip'
    if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) { Stop-Test 'archive was not created' }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($archive)
    try {
        $names = @($zip.Entries | ForEach-Object FullName)
        foreach ($required in @('henosis-0.1.0-alpha.6-x86_64-pc-windows-msvc/HENOSIS_ARCHIVE', 'henosis-0.1.0-alpha.6-x86_64-pc-windows-msvc/crucible.exe', 'henosis-0.1.0-alpha.6-x86_64-pc-windows-msvc/henosis.exe', 'henosis-0.1.0-alpha.6-x86_64-pc-windows-msvc/install.ps1')) {
            if ($names -notcontains $required) { Stop-Test "archive is missing $required" }
        }
        $marker = $zip.GetEntry('henosis-0.1.0-alpha.6-x86_64-pc-windows-msvc/HENOSIS_ARCHIVE')
        $reader = [System.IO.StreamReader]::new($marker.Open())
        try {
            if ($reader.ReadToEnd() -ne 'v0.1.0-alpha.6 x86_64-pc-windows-msvc') { Stop-Test 'archive marker is incorrect' }
        }
        finally { $reader.Dispose() }
    }
    finally { $zip.Dispose() }
    Write-Output 'Windows release package contract passed'
}
finally { Clear-TestRoot }
