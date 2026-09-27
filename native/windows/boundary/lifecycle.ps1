param(
  [Parameter(Mandatory=$true)][string]$Binaries,
  [Parameter(Mandatory=$true)][string]$FixtureRoot
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = [IO.Path]::GetFullPath($FixtureRoot)
if (Test-Path -LiteralPath $root) { throw 'Use a new disposable fixture directory' }
$workspace = Join-Path $root 'workspace'
$runtime = Join-Path $root 'runtime'
New-Item -ItemType Directory $workspace,$runtime | Out-Null
foreach ($file in @('latch-boundary-probe.exe','latch-boundary-files.exe','latch-boundary-compat.dll')) {
  Copy-Item -LiteralPath (Join-Path $Binaries $file) -Destination $runtime
}
$runner = Join-Path $runtime 'latch-boundary-probe.exe'
$fixture = Join-Path $runtime 'latch-boundary-files.exe'
$baselineWorkspace = (Get-Acl -LiteralPath $workspace).Sddl
$baselineRuntime = (Get-Acl -LiteralPath $runtime).Sddl
function Assert-Cleanup([string]$packageSid = '', [string]$packageName = '') {
  $deadline = [DateTime]::UtcNow.AddSeconds(15)
  do {
    $clean = (Get-Acl -LiteralPath $workspace).Sddl -eq $baselineWorkspace -and
      (Get-Acl -LiteralPath $runtime).Sddl -eq $baselineRuntime -and
      !(Test-Path -LiteralPath (Join-Path $workspace '.git'))
    if ($packageSid) {
      $mapping = 'HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings\' + $packageSid
      $clean = $clean -and !(Test-Path -LiteralPath $mapping)
    }
    if ($packageName) {
      $clean = $clean -and !(Test-Path -LiteralPath (Join-Path $env:LOCALAPPDATA ('Packages/' + $packageName)))
    }
    if ($clean) { return }
    Start-Sleep -Milliseconds 20
  } while ([DateTime]::UtcNow -lt $deadline)
  throw 'Temporary grants, .git reservation, or AppContainer profile survived cleanup'
}
foreach ($mode in @('timeout','parent-exit','drop')) {
  $marker = Join-Path $workspace $mode
  $operation = if ($mode -eq 'parent-exit') { 'tree-root-exit' } else { 'tree' }
  $timeout = if ($mode -eq 'timeout') { 1500 } else { 8000 }
  $info = New-Object Diagnostics.ProcessStartInfo
  $info.FileName = $runner
  $info.UseShellExecute = $false
  $info.CreateNoWindow = $true
  $q = [char]34
  $info.Arguments = $q+$workspace+$q+' '+$q+$fixture+$q+' '+$q+$operation+' 3 '+$marker+$q+
    ' write --read-root '+$q+$runtime+$q+' --timeout-ms '+$timeout+' --protect-git '+$q+$workspace+$q
  $process = [Diagnostics.Process]::Start($info)
  $descendants = @()
  try {
    $deadline = [DateTime]::UtcNow.AddSeconds(6)
    while (!(Test-Path -LiteralPath ($marker+'.0'))) {
      if ($process.HasExited -or [DateTime]::UtcNow -gt $deadline) { throw 'Tree failed to start' }
      Start-Sleep -Milliseconds 20
    }
    foreach ($generation in 0..3) {
      $childId = [BitConverter]::ToInt32([IO.File]::ReadAllBytes($marker+'.'+$generation),0)
      $descendants += [Diagnostics.Process]::GetProcessById($childId)
    }
    # Read the SID recorded by the real target. Short-lived jobs can finish
    # before a host ACL lookup returns; this does not race grant removal.
    $packageSid = [IO.File]::ReadAllText($marker+'.package')
    if (!$packageSid.StartsWith('S-1-15-2-')) { throw 'Target was not in an AppContainer' }
    $mapping = 'HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings\' + $packageSid
    $packageName = ''
    if (Test-Path -LiteralPath $mapping) {
      $packageName = (Get-ItemProperty -LiteralPath $mapping).Moniker
      if (!$packageName.StartsWith('LatchProbe.', [StringComparison]::OrdinalIgnoreCase)) { throw 'Unexpected AppContainer profile' }
    }
    if ($mode -eq 'drop') { $process.Kill() }
    if (!$process.WaitForExit(5000)) { throw ('Runner survived '+$mode) }
    if ($mode -eq 'timeout' -and $process.ExitCode -ne 124) { throw 'Timeout did not return 124' }
    if ($mode -eq 'parent-exit' -and $process.ExitCode -ne 0) { throw 'Parent exit failed' }
    foreach ($child in $descendants) {
      if (!$child.WaitForExit(2000)) { throw ('Descendant survived '+$mode+': '+$child.Id) }
    }
    Assert-Cleanup $packageSid $packageName
    Write-Output ('PASS four-generation '+$mode+' and ACL/profile cleanup')
  } finally {
    if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
    $process.Dispose()
    foreach ($child in $descendants) { $child.Dispose() }
  }
}

# Failure after profile/grant setup must follow the same cleanup path.
& $runner $workspace (Join-Path $runtime 'absent.exe') 'unused' write --read-root $runtime --protect-git $workspace
if ($LASTEXITCODE -ne 125) { throw 'Missing executable did not fail closed' }
Assert-Cleanup
Write-Output 'PASS failed-create ACL/reservation cleanup'
$global:LASTEXITCODE=0
