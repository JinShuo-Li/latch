param(
  [Parameter(Mandatory=$true)][string]$Binaries,
  [Parameter(Mandatory=$true)][string]$FixtureRoot,
  [ValidateRange(1024,100000)][int]$ObjectCount = 4096,
  [ValidateRange(1,4096)][int]$SandboxMutationCount = 256,
  [ValidateRange(1,1024)][int]$ConcurrentHostFilesPerBranch = 32
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = [IO.Path]::GetFullPath($FixtureRoot)
if (Test-Path -LiteralPath $root) { throw 'Use a new disposable fixture directory' }
$workspace = Join-Path $root 'workspace'
$runtime = Join-Path $root 'runtime'
$journal = Join-Path $root 'journal'
New-Item -ItemType Directory -Path $root,$workspace,$runtime,$journal | Out-Null
foreach ($file in @('latch-boundary-probe.exe','latch-boundary-files.exe','latch-boundary-compat.dll')) {
  Copy-Item -LiteralPath (Join-Path $Binaries $file) -Destination $runtime
}
$runner = Join-Path $runtime 'latch-boundary-probe.exe'
$fixture = Join-Path $runtime 'latch-boundary-files.exe'
$workspaceAcl = (Get-Acl -LiteralPath $workspace).Sddl
$runtimeAcl = (Get-Acl -LiteralPath $runtime).Sddl
$branchCount = 16
$filesPerBranch = [Math]::Ceiling($ObjectCount / $branchCount)
$branches = @()
$templates = @{}
$originalAcls = [Collections.Generic.Dictionary[string,string]]::new([StringComparer]::OrdinalIgnoreCase)
for ($branch = 0; $branch -lt $branchCount; $branch++) {
  $directory = Join-Path $workspace ('branch-{0:D2}' -f $branch)
  New-Item -ItemType Directory -Path $directory | Out-Null
  $branches += $directory
  $originalAcls[$directory] = (Get-Acl -LiteralPath $directory).Sddl
  $template = Join-Path $directory 'baseline-template.txt'
  [IO.File]::WriteAllText($template, 'baseline')
  $templates[$directory] = $template
  $originalAcls[$template] = (Get-Acl -LiteralPath $template).Sddl
  for ($item = 0; $item -lt $filesPerBranch; $item++) {
    $path = Join-Path $directory ('seed-{0:D5}.txt' -f $item)
    [IO.File]::WriteAllText($path, 'seed')
    $originalAcls[$path] = (Get-Acl -LiteralPath $path).Sddl
  }
}
$originalAcls[$workspace] = $workspaceAcl
$sample = Join-Path $branches[0] 'seed-00000.txt'
$command = 'bulk-mutate "' + $branches[0] + '" ' + $SandboxMutationCount
$q = [char]34
$info = New-Object Diagnostics.ProcessStartInfo
$info.FileName = $runner
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
$info.RedirectStandardOutput = $true
$info.RedirectStandardError = $true
$escapedCommand = $command.Replace('"','\"')
$info.Arguments = $q+$workspace+$q+' '+$q+$fixture+$q+' '+$q+$escapedCommand+$q+
  ' write --read-root '+$q+$runtime+$q+' --timeout-ms 120000'
$knownMappings = @{}
$mappingRoot = 'HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings'
if (Test-Path -LiteralPath $mappingRoot) {
  foreach ($mapping in Get-ChildItem -LiteralPath $mappingRoot) { $knownMappings[$mapping.PSChildName] = $true }
}
$savedEnvironment = @{}
foreach ($key in @('LATCH_RECOVERY_ROOT','LATCH_RECOVERY_PAUSE','LATCH_BOUNDARY_TIMING')) {
  $savedEnvironment[$key] = [Environment]::GetEnvironmentVariable($key)
}
$env:LATCH_RECOVERY_ROOT = $journal
$env:LATCH_RECOVERY_PAUSE = 'all-grants'
$env:LATCH_BOUNDARY_TIMING = '1'
$info.EnvironmentVariables['LATCH_RECOVERY_ROOT'] = $journal
$info.EnvironmentVariables['LATCH_RECOVERY_PAUSE'] = 'all-grants'
$info.EnvironmentVariables['LATCH_BOUNDARY_TIMING'] = '1'
$process = [Diagnostics.Process]::Start($info)
$stdoutTask = $process.StandardOutput.ReadToEndAsync()
$stderrTask = $process.StandardError.ReadToEndAsync()
$hostFiles = @()
try {
  $pauseFile = Join-Path $journal 'pause.pid'
  $deadline = [DateTime]::UtcNow.AddMinutes(30)
  while (!(Test-Path -LiteralPath $pauseFile)) {
    if ($process.HasExited) {
      $diagnostics = @($stdoutTask.Result, $stderrTask.Result | Where-Object { $_ }) -join "`n"
      throw "Large-workspace runner exited before the post-grant checkpoint (exit $($process.ExitCode)); $diagnostics"
    }
    if ([DateTime]::UtcNow -gt $deadline) {
      throw 'Large-workspace runner did not reach the post-grant checkpoint'
    }
    Start-Sleep -Milliseconds 50
  }
  $packageSid = $null
  foreach ($mapping in Get-ChildItem -LiteralPath $mappingRoot) {
    if ($knownMappings.ContainsKey($mapping.PSChildName)) { continue }
    $properties = Get-ItemProperty -LiteralPath $mapping.PSPath
    if ($properties.Moniker -like 'LatchProbe.*') {
      $packageSid = $mapping.PSChildName
      break
    }
  }
  if (!$packageSid) { throw 'Could not identify the active AppContainer profile' }

  # Add files from the host after the runner has journaled its initial tree.
  # They inherit the temporary package ACE and must be cleaned during rollback.
  foreach ($directory in $branches) {
    for ($item = 0; $item -lt $ConcurrentHostFilesPerBranch; $item++) {
      $path = Join-Path $directory ('host-{0:D4}.txt' -f $item)
      [IO.File]::WriteAllText($path, 'host mutation')
      $hostFiles += $path
    }
  }
  New-Item -ItemType File -Path (Join-Path $journal 'pause.resume') | Out-Null
  if (!$process.WaitForExit(1800000)) { throw 'Large-workspace runner exceeded 30 minutes' }
  $stdout = $stdoutTask.Result
  $stderr = $stderrTask.Result
  if ($stdout) { Write-Output $stdout }
  if ($stderr) { Write-Output $stderr }
  if ($process.ExitCode) { throw "Large-workspace command exited $($process.ExitCode)" }

  foreach ($path in $originalAcls.Keys) {
    if (!(Test-Path -LiteralPath $path)) { throw "Original workspace object disappeared: $path" }
    if ((Get-Acl -LiteralPath $path).Sddl -ne $originalAcls[$path]) {
      throw "Original ACL was not restored exactly: $path"
    }
  }
  foreach ($path in $hostFiles) {
    if (!(Test-Path -LiteralPath $path)) { throw "Concurrent host file disappeared: $path" }
    $expected = $originalAcls[$templates[(Split-Path -Parent $path)]]
    if ((Get-Acl -LiteralPath $path).Sddl -ne $expected) {
      throw "Concurrent host file did not return to its inherited ACL: $path"
    }
  }
  for ($item = 0; $item -lt $SandboxMutationCount; $item++) {
    $path = Join-Path $branches[0] ('sandbox-{0}.txt' -f $item)
    if ($item % 4 -eq 1) {
      if (Test-Path -LiteralPath $path) { throw "Sandbox-deleted file survived: $path" }
      continue
    }
    $finalPath = if ($item % 2 -eq 0) { $path + '.renamed' } else { $path }
    if (!(Test-Path -LiteralPath $finalPath)) { throw "Sandbox-created object is missing: $finalPath" }
    if ((Get-Acl -LiteralPath $finalPath).Sddl -ne $originalAcls[$templates[$branches[0]]]) {
      throw "Sandbox-created object retained temporary grants: $finalPath"
    }
  }
  if ((Get-Acl -LiteralPath $workspace).Sddl -ne $workspaceAcl -or
      (Get-Acl -LiteralPath $runtime).Sddl -ne $runtimeAcl) {
    throw 'Large-workspace fixture did not restore root ACLs exactly'
  }
  if (Test-Path -LiteralPath (Join-Path $journal 'pending')) {
    throw 'Large-workspace rollback retained its journal'
  }
  & $runner --recover-only
  if ($LASTEXITCODE) { throw "Idempotent recovery check exited $LASTEXITCODE" }
  Write-Output "PASS large mutable workspace: $($originalAcls.Count) initial objects, $($hostFiles.Count) concurrent host files, $SandboxMutationCount sandbox mutations"
} finally {
  if (!$process.HasExited) {
    New-Item -ItemType File -Path (Join-Path $journal 'pause.resume') -Force | Out-Null
    if (!$process.WaitForExit(30000)) { $process.Kill(); $process.WaitForExit() }
  }
  $process.Dispose()
  foreach ($key in $savedEnvironment.Keys) {
    [Environment]::SetEnvironmentVariable($key, $savedEnvironment[$key])
  }
}
$global:LASTEXITCODE = 0
