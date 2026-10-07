# Usage: irm https://jinshuo-li.github.io/latch/install.ps1 | iex
# Pin: download this script, then ./install.ps1 -Version v0.3.0
[CmdletBinding()]
param(
    [string]$Version = $(if ($env:LATCH_VERSION) { $env:LATCH_VERSION } else { 'latest' }),
    [string]$InstallDir = $(if ($env:LATCH_INSTALL_DIR) { $env:LATCH_INSTALL_DIR } elseif ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA 'Programs\Latch\bin' } else { '' })
)

function Install-Latch {
    $ErrorActionPreference = 'Stop'
    $repo = 'https://github.com/JinShuo-Li/latch'
    $work = $null
    $staged = $null
    try {
        if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
            throw 'Windows is required. On Linux use install.sh; macOS is not supported.'
        }
        # PROCESSOR_ARCHITEW6432 identifies the native OS from a 32-bit PowerShell host.
        $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
        if ($arch -ne 'AMD64') { throw "Unsupported architecture: $arch. Windows releases currently require x86_64." }
        if ([string]::IsNullOrWhiteSpace($InstallDir)) { throw 'Install directory cannot be empty.' }
        # Windows PowerShell 5.1 may otherwise negotiate obsolete TLS versions.
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        if ($Version -eq 'latest') {
            Write-Host 'Resolving the latest Latch release...'
            # Follow the public release redirect, avoiding the unauthenticated API quota.
            $release = Invoke-WebRequest -UseBasicParsing -Method Head -Uri "$repo/releases/latest" -TimeoutSec 120
            # Windows PowerShell 5.1 uses HttpWebResponse; PowerShell 7 uses HttpResponseMessage.
            $releaseUri = if ($release.BaseResponse.ResponseUri) { $release.BaseResponse.ResponseUri }
                          else { $release.BaseResponse.RequestMessage.RequestUri }
            $releaseUrl = [string]$releaseUri
            $tagPrefix = "$repo/releases/tag/"
            if (-not $releaseUrl.StartsWith($tagPrefix, [StringComparison]::Ordinal)) {
                throw 'GitHub did not return a release tag. Try setting LATCH_VERSION to vX.Y.Z.'
            }
            $Version = $releaseUrl.Substring($tagPrefix.Length)
        }
        if (-not $Version.StartsWith('v')) { $Version = "v$Version" }
        if ($Version -cnotmatch '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$') { throw "Invalid version: $Version. Expected vX.Y.Z (or a prerelease tag)." }
        $asset = 'latch-x86_64-pc-windows-msvc.zip'
        $base = "$repo/releases/download/$Version"
        $work = Join-Path ([IO.Path]::GetTempPath()) ('latch-install-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $work | Out-Null
        Write-Host "Downloading Latch $Version for x86_64 Windows..."
        $archive = Join-Path $work $asset
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $archive -TimeoutSec 300
        $manifest = Join-Path $work 'SHA256SUMS'
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $manifest -TimeoutSec 120
        $pattern = '^([0-9a-fA-F]{64})\s+\*?' + [regex]::Escape($asset) + '$'
        $checksums = @(Get-Content -LiteralPath $manifest | ForEach-Object { if ($_ -match $pattern) { $Matches[1] } })
        if ($checksums.Count -ne 1) { throw "Missing or invalid checksum for $asset. Nothing was installed." }
        $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
        if ($actual -ne $checksums[0]) { throw 'SHA256 mismatch. Nothing was installed; retry or report the release.' }
        # Extract only the named binary, never arbitrary paths from the archive.
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        try {
            $entries = @($zip.Entries | Where-Object { $_.FullName -ceq 'latch.exe' })
            if ($entries.Count -ne 1) { throw 'Release archive must contain exactly one latch.exe.' }
            $binary = Join-Path $work 'latch.exe'
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entries[0], $binary, $false)
        } finally { $zip.Dispose() }
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        $destination = Join-Path $InstallDir 'latch.exe'
        if (Test-Path -LiteralPath $destination -PathType Container) { throw "$destination is a directory. Choose another -InstallDir." }
        $staged = Join-Path $InstallDir ('.latch-install-' + [Guid]::NewGuid().ToString('N') + '.exe')
        Copy-Item -LiteralPath $binary -Destination $staged
        # Replacement leaves an existing binary intact if Windows has it open.
        if (Test-Path -LiteralPath $destination) { [IO.File]::Replace($staged, $destination, [NullString]::Value) }
        else { [IO.File]::Move($staged, $destination) }
        $staged = $null
        Write-Host "Installed Latch $Version to $destination (SHA256 verified)."
        if (($env:PATH -split ';') -notcontains $InstallDir) {
            Write-Host "Add $InstallDir to your user PATH using Windows Environment Variables, then reopen your terminal."
            Write-Host ('For this terminal: $env:PATH = ' + "'$($InstallDir.Replace("'", "''"));'" + ' + $env:PATH')
        }
        Write-Host 'Runtime: Git for Windows, ripgrep (rg), and a user-owned NTFS workspace. No MSVC or WSL needed.'
        Write-Host 'Next: run latch, use /setup to configure a provider, then run latch doctor.'
    } catch {
        throw "Latch installer: $($_.Exception.Message) Check your connection and $repo/releases. If no binaries are published yet, see docs/INSTALL.md for a source build. Close running Latch processes before upgrading."
    } finally {
        if ($staged -and (Test-Path -LiteralPath $staged)) { Remove-Item -LiteralPath $staged -Force }
        if ($work -and (Test-Path -LiteralPath $work)) { Remove-Item -LiteralPath $work -Recurse -Force }
    }
}

# Keep invocation last so a partially downloaded script does not start installing.
Install-Latch
