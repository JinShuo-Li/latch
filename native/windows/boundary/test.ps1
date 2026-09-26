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
$state = Join-Path $root 'state'
$external = Join-Path $root 'external'
$git = Join-Path $workspace '.git'
New-Item -ItemType Directory $workspace,$runtime,$state,$external,$git | Out-Null
Copy-Item -LiteralPath (Join-Path $Binaries 'latch-boundary-probe.exe') -Destination $runtime
Copy-Item -LiteralPath (Join-Path $Binaries 'latch-boundary-compat.dll') -Destination $runtime
Copy-Item -LiteralPath (Join-Path $Binaries 'latch-boundary-files.exe') -Destination $runtime
$runner = Join-Path $runtime 'latch-boundary-probe.exe'
$fixture = Join-Path $runtime 'latch-boundary-files.exe'
$readable = Join-Path $workspace 'read.txt'
$secret = Join-Path $state 'secrets.toml'
$gitFile = Join-Path $git 'config'
$outside = Join-Path $root 'outside.txt'
foreach ($path in @($readable,$secret,$gitFile,$outside)) { [IO.File]::WriteAllText($path,'fixture') }
& icacls.exe $outside /grant '*S-1-15-2-1:(M)' '*S-1-5-32-545:(M)' | Out-Null
if ($LASTEXITCODE) { throw 'Fixture ACL setup failed' }
$base = @('--read-root',$runtime,'--deny',$state)
function Check([string]$operation,[string]$path,[string]$mode,[string[]]$extra = @()) {
  & $runner $workspace $fixture "$operation `"$path`"" $mode @base @extra
  if ($LASTEXITCODE) { throw "Subprocess assertion failed: $operation $path ($LASTEXITCODE)" }
}
Check 'read-allow' $readable 'read'
Check 'write-deny' $readable 'read'
Check 'create-deny' (Join-Path $workspace 'read-only-new.txt') 'read'
Check 'write-allow' $readable 'write'
Check 'create-allow' (Join-Path $workspace 'new.txt') 'write'
Check 'read-deny' $secret 'write'
Check 'write-deny' $secret 'write'
Check 'acl-deny' $secret 'write'
Check 'read-allow' $outside 'write'
Check 'write-deny' $outside 'write'
Check 'acl-deny' $outside 'write'
Check 'create-deny' (Join-Path $external 'denied.txt') 'write'
Check 'create-allow' (Join-Path $external 'allowed.txt') 'write' @('--write-root',$external)
Check 'write-deny' $gitFile 'write' @('--deny-write',$git)
Check 'read-allow' $gitFile 'write' @('--deny-write',$git)
Check 'delete-deny' $git 'write' @('--deny-write',$git)
Check 'acl-deny' $gitFile 'write' @('--deny-write',$git)
Check 'write-allow' $gitFile 'write'

# The runner owns the job; terminating it must kill four generations.
$marker = Join-Path $workspace 'tree'
$info = New-Object Diagnostics.ProcessStartInfo
$info.FileName = $runner
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
$info.Arguments = "`"$workspace`" `"$fixture`" `"tree 3 $marker`" write --read-root `"$runtime`" --deny `"$state`""
$process = [Diagnostics.Process]::Start($info)
$descendants = @()
try {
  $deadline = [DateTime]::UtcNow.AddSeconds(10)
  while (!(Test-Path -LiteralPath "$marker.0")) {
    if ($process.HasExited -or [DateTime]::UtcNow -gt $deadline) { throw 'Tree failed to start' }
    Start-Sleep -Milliseconds 50
  }
  foreach ($generation in 0..3) {
    $childId = [BitConverter]::ToInt32([IO.File]::ReadAllBytes("$marker.$generation"),0)
    $descendants += [Diagnostics.Process]::GetProcessById($childId)
  }
  $process.Kill()
  if (!$process.WaitForExit(5000)) { throw 'Runner did not exit' }
  foreach ($child in $descendants) {
    if (!$child.WaitForExit(5000)) { throw "Descendant survived: $($child.Id)" }
  }
  'PASS four-generation tree teardown'
} finally {
  if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
  $process.Dispose()
  foreach ($child in $descendants) { $child.Dispose() }
}
'PASS initial boundary matrix'
