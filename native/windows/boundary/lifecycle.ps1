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
    if ($mode -eq 'drop') { $process.Kill() }
    if (!$process.WaitForExit(5000)) { throw ('Runner survived '+$mode) }
    if ($mode -eq 'timeout' -and $process.ExitCode -ne 124) { throw 'Timeout did not return 124' }
    if ($mode -eq 'parent-exit' -and $process.ExitCode -ne 0) { throw 'Parent exit failed' }
    foreach ($child in $descendants) {
      if (!$child.WaitForExit(2000)) { throw ('Descendant survived '+$mode+': '+$child.Id) }
    }
    if (Test-Path -LiteralPath (Join-Path $workspace '.git')) { throw 'Metadata reservation survived teardown' }
    Write-Output ('PASS four-generation '+$mode)
  } finally {
    if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
    $process.Dispose()
    foreach ($child in $descendants) { $child.Dispose() }
  }
}
