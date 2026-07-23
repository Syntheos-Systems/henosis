# Verify the Windows installer keeps its checksum, rollback, and initialization contracts visible.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot
$installer = Join-Path $repository 'install.ps1'

# Stop the Windows installer contract with a diagnostic.
function Stop-Test {
    param([Parameter(Mandatory = $true)][string]$Message)
    throw "FAIL: $Message"
}

# Require a fixed installer contract string.
function Assert-InstallerText {
    param([Parameter(Mandatory = $true)][string]$Text)
    if ((Get-Content -LiteralPath $installer -Raw).IndexOf($Text, [StringComparison]::Ordinal) -lt 0) { Stop-Test "installer is missing: $Text" }
}

$tokens = $null
$parseErrors = $null
[System.Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$parseErrors) | Out-Null
if ($parseErrors.Count -ne 0) { Stop-Test "installer has $($parseErrors.Count) PowerShell syntax error(s)" }

Assert-InstallerText 'Get-FileHash -Algorithm SHA256'
Assert-InstallerText '& $destination init --quick'
Assert-InstallerText 'Move-Item -LiteralPath $backup -Destination $destination -Force'
Assert-InstallerText "return 'x86_64-pc-windows-msvc'"
Assert-InstallerText 'ConvertTo-Json -Compress'
Assert-InstallerText '$uri.Scheme -ne [Uri]::UriSchemeHttps'
Assert-InstallerText 'without credentials, query, or fragment'
Assert-InstallerText '[Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT'
Assert-InstallerText 'ConvertFrom-Json'
Assert-InstallerText 'selected release is not immutable'
Assert-InstallerText "Join-Path `$PSScriptRoot 'HENOSIS_ARCHIVE'"
Assert-InstallerText "Join-Path `$PSScriptRoot 'henosis.exe'"
Assert-InstallerText 'release archive marker does not match this installer'
Write-Output 'Windows installer contract passed'
