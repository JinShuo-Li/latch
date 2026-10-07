# Offline Windows installer checks; no provider, network, or PATH changes.
$ErrorActionPreference = 'Stop'
$installer = Join-Path $PSScriptRoot 'install.ps1'
$root = Join-Path ([IO.Path]::GetTempPath()) ('latch-installer-test-' + [Guid]::NewGuid().ToString('N'))
$originalArch = $env:PROCESSOR_ARCHITECTURE
$originalWowArch = $env:PROCESSOR_ARCHITEW6432
$originalVersion = $env:LATCH_VERSION
$originalInstallDir = $env:LATCH_INSTALL_DIR
$originalPath = $env:PATH
$global:LatchFixture = $root
$global:LatchRequests = @()
$global:LatchReleaseUrl = 'https://github.com/JinShuo-Li/latch/releases/tag/v9.8.7'
$global:LatchResponseStyle = 'WindowsPowerShell'

function global:Invoke-RestMethod {
    param($Uri, $Headers, $TimeoutSec)
    throw 'GitHub API rate limit exceeded: installer must not use the API'
}
function global:Invoke-WebRequest {
    param([switch]$UseBasicParsing, $Method, $Uri, $OutFile, $TimeoutSec)
    $global:LatchRequests += $Uri
    if ($Uri -eq 'https://github.com/JinShuo-Li/latch/releases/latest') {
        if ($Method -ne 'Head') { throw 'Release lookup must use HEAD' }
        $redirectUri = if ($global:LatchReleaseUrl) { [uri]$global:LatchReleaseUrl } else { $null }
        if ($global:LatchResponseStyle -eq 'WindowsPowerShell') {
            return @{ BaseResponse = @{ ResponseUri = $redirectUri } }
        }
        return @{ BaseResponse = @{ RequestMessage = @{ RequestUri = $redirectUri } } }
    }
    if (-not $Uri.StartsWith('https://github.com/JinShuo-Li/latch/releases/download/v9.8.7/')) { throw "Unexpected URL: $Uri" }
    Copy-Item -LiteralPath (Join-Path $global:LatchFixture ($Uri.Split('/')[-1])) -Destination $OutFile
}
function Assert-True($condition, $message) { if (-not $condition) { throw $message } }
function New-Fixture($entryName = 'latch.exe') {
    $archive = Join-Path $root 'latch-x86_64-pc-windows-msvc.zip'
    if (Test-Path $archive) { Remove-Item $archive }
    $file = Join-Path $root $entryName
    Set-Content -LiteralPath $file -Value 'new fixture binary' -NoNewline
    Compress-Archive -LiteralPath $file -DestinationPath $archive
    $digest = (Get-FileHash $archive -Algorithm SHA256).Hash
    "$digest  latch-x86_64-pc-windows-msvc.zip" | Set-Content (Join-Path $root 'SHA256SUMS')
}
function Run-Case($name, $arguments = @{}, $expectError = $null, $piped = $false) {
    $destination = Join-Path $root 'user bin'
    New-Item -ItemType Directory $destination -Force | Out-Null
    $binary = Join-Path $destination 'latch.exe'
    Set-Content $binary 'existing installation' -NoNewline
    $global:LatchRequests = @()
    $errorMessage = $null
    $before = @(Get-ChildItem ([IO.Path]::GetTempPath()) -Directory -Filter 'latch-install-*' | ForEach-Object { $_.FullName })
    try {
        if ($piped) {
            $env:LATCH_INSTALL_DIR = $destination
            try { Invoke-Expression (Get-Content -LiteralPath $installer -Raw) }
            finally { $env:LATCH_INSTALL_DIR = '' }
        } else { & $installer -InstallDir $destination @arguments }
    } catch { $errorMessage = $_.Exception.Message }
    if ($expectError) {
        Assert-True ($errorMessage -and $errorMessage.Contains($expectError)) "$name did not fail as expected: $errorMessage"
        Assert-True ((Get-Content $binary -Raw) -eq 'existing installation') "$name changed an existing binary on failure"
    } else {
        Assert-True (-not $errorMessage) "$name failed: $errorMessage"
        Assert-True ((Get-Content $binary -Raw) -eq 'new fixture binary') "$name did not install the binary"
    }
    $after = @(Get-ChildItem ([IO.Path]::GetTempPath()) -Directory -Filter 'latch-install-*' | ForEach-Object { $_.FullName })
    Assert-True (($after -join '|') -eq ($before -join '|')) "$name leaked downloaded files"
    Assert-True (-not @(Get-ChildItem $destination -Filter '.latch-install-*').Count) "$name leaked a staged binary"
    Assert-True ($env:PATH -eq $originalPath) "$name changed PATH"
    Write-Host "PASS: $name"
}
try {
    New-Item -ItemType Directory $root | Out-Null
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    $env:PROCESSOR_ARCHITEW6432 = ''
    $env:LATCH_VERSION = ''
    $env:LATCH_INSTALL_DIR = ''
    New-Fixture
    Run-Case 'latest / upgrade'
    Assert-True ($global:LatchRequests.Count -eq 3) 'Latest should resolve once and download two files'
    Run-Case 'one-liner / environment install directory' @{} $null $true
    Assert-True ($global:LatchRequests[0] -eq 'https://github.com/JinShuo-Li/latch/releases/latest') 'One-liner must use the public release redirect'
    $global:LatchResponseStyle = 'PowerShell7'
    Run-Case 'PowerShell 7 release redirect' @{} $null $true
    $global:LatchResponseStyle = 'WindowsPowerShell'
    foreach ($url in @('https://example.com/releases/tag/v9.8.7', 'https://github.com/JinShuo-Li/latch/releases', '')) {
        $global:LatchReleaseUrl = $url
        Run-Case 'unexpected release redirect' @{} 'GitHub did not return a release tag'
        Assert-True ($global:LatchRequests.Count -eq 1) 'Invalid redirect must not download assets'
    }
    $global:LatchReleaseUrl = 'https://github.com/JinShuo-Li/latch/releases/tag/invalid'
    Run-Case 'invalid release tag' @{} 'Invalid version'
    Assert-True ($global:LatchRequests.Count -eq 1) 'Invalid release tag must not download assets'
    $global:LatchReleaseUrl = 'https://github.com/JinShuo-Li/latch/releases/tag/v9.8.7'
    Run-Case 'pinned version' @{ Version = '9.8.7' }
    Assert-True ($global:LatchRequests.Count -eq 2) 'Pinned version must not query latest'
    $env:LATCH_VERSION = 'v9.8.7'
    Run-Case 'environment version'
    $env:LATCH_VERSION = ''
    Run-Case 'invalid version' @{ Version = '../bad' } 'Invalid version'
    Assert-True ($global:LatchRequests.Count -eq 0) 'Invalid version must not download'
    $env:PROCESSOR_ARCHITECTURE = 'ARM64'
    Run-Case 'unsupported architecture' @{} 'Unsupported architecture'
    Assert-True ($global:LatchRequests.Count -eq 0) 'Unsupported architecture must not download'
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    Set-Content (Join-Path $root 'latch-x86_64-pc-windows-msvc.zip') 'corrupt'
    Run-Case 'checksum mismatch' @{} 'SHA256 mismatch'
    New-Fixture
    $manifest = Join-Path $root 'SHA256SUMS'
    Add-Content $manifest (Get-Content $manifest)
    Run-Case 'duplicate checksum' @{} 'Missing or invalid checksum'
    'invalid checksum' | Set-Content $manifest
    Run-Case 'malformed checksum' @{} 'Missing or invalid checksum'
    Remove-Item $manifest
    Run-Case 'missing checksum' @{} 'Cannot find path'
    New-Fixture 'other.exe'
    Run-Case 'missing binary' @{} 'exactly one latch.exe'
    New-Fixture
    Remove-Item (Join-Path $root 'latch-x86_64-pc-windows-msvc.zip')
    Run-Case 'missing release asset' @{} 'Cannot find path'
    Write-Host 'All Windows installer checks passed.'
} finally {
    $env:PROCESSOR_ARCHITECTURE = $originalArch
    $env:PROCESSOR_ARCHITEW6432 = $originalWowArch
    $env:LATCH_VERSION = $originalVersion
    $env:LATCH_INSTALL_DIR = $originalInstallDir
    Remove-Item Function:\Invoke-RestMethod, Function:\Invoke-WebRequest
    Remove-Variable LatchFixture, LatchRequests, LatchReleaseUrl, LatchResponseStyle -Scope Global
    if (Test-Path $root) { Remove-Item $root -Recurse -Force }
}
