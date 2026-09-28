param([Parameter(Mandatory=$true)][string]$Binaries,[Parameter(Mandatory=$true)][string]$FixtureRoot)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$root=[IO.Path]::GetFullPath($FixtureRoot)
if(Test-Path -LiteralPath $root){throw 'Use a new disposable fixture'}
$workspace=Join-Path $root 'workspace'
$runtime=Join-Path $root 'runtime'
New-Item -ItemType Directory $workspace,$runtime | Out-Null
foreach($name in @('latch-boundary-probe.exe','latch-boundary-files.exe','latch-boundary-compat.dll')){Copy-Item -LiteralPath (Join-Path $Binaries $name) -Destination $runtime}
$runner=Join-Path $runtime 'latch-boundary-probe.exe'
$fixture=Join-Path $runtime 'latch-boundary-files.exe'
$file=Join-Path $workspace 'ordinary.txt'
[IO.File]::WriteAllText($file,'fixture')
$baseline=(Get-Acl -LiteralPath $runtime).Sddl
$journal=Join-Path $root 'journal'
$info=[Diagnostics.ProcessStartInfo]::new()
$info.FileName=$runner
$info.UseShellExecute=$false
$info.CreateNoWindow=$true
$q=[char]34
$info.Arguments=$q+$workspace+$q+' '+$q+$fixture+$q+' '+$q+'read-allow '+$file+$q+' write --read-root '+$q+$runtime+$q+' --protect-git '+$q+$workspace+$q
$info.EnvironmentVariables['LATCH_RECOVERY_ROOT']=$journal
$info.EnvironmentVariables['LATCH_RECOVERY_PAUSE']='descendants'
$launcher=[Diagnostics.Process]::Start($info)
$owner=$null
try{
  $signal=Join-Path $journal 'pause.pid'
  $deadline=[DateTime]::UtcNow.AddSeconds(20)
  while(!(Test-Path -LiteralPath $signal)){if($launcher.HasExited -or [DateTime]::UtcNow -gt $deadline){throw 'Execution checkpoint not reached'};Start-Sleep -Milliseconds 20}
  $ownerId=[BitConverter]::ToInt32([IO.File]::ReadAllBytes($signal),0)
  $owner=[Diagnostics.Process]::GetProcessById($ownerId)
  $owner.Kill()
  if(!$owner.WaitForExit(5000)){throw 'Cleanup owner survived'}
  if(!$launcher.WaitForExit(5000)){throw 'Launcher survived'}
  $expected=[IO.Path]::GetFullPath((Join-Path $root 'workspace'))
  if($workspace -cne $expected -or [IO.Path]::GetDirectoryName($workspace) -cne $root){throw 'Unsafe fixture deletion path'}
  # The job closes on owner death, but Windows can keep a terminating child
  # handle open briefly. The root must still become deletable before recovery.
  $deleteDeadline=[DateTime]::UtcNow.AddSeconds(10)
  while($true){
    try{
      Remove-Item -LiteralPath $workspace -Recurse -Force
      if(!(Test-Path -LiteralPath $workspace)){break}
    }catch{
      if([DateTime]::UtcNow -ge $deleteDeadline){throw}
    }
    if([DateTime]::UtcNow -ge $deleteDeadline){throw 'Sandbox descendants still hold the deleted-root fixture'}
    Start-Sleep -Milliseconds 50
  }
  $env:LATCH_RECOVERY_ROOT=$journal
  & $runner --recover-only
  if($LASTEXITCODE){throw ('Deleted-root recovery failed: '+$LASTEXITCODE)}
  if(Test-Path -LiteralPath (Join-Path $journal 'pending')){throw 'Recovery journal retained'}
  if((Get-Acl -LiteralPath $runtime).Sddl -ne $baseline){throw 'Runtime ACL not restored'}
  $env:LATCH_RECOVERY_ROOT=$journal
  & $runner --recover-only
  if($LASTEXITCODE){throw 'Deleted-root recovery not idempotent'}
  Write-Output 'PASS deleted workspace exact-ID recovery and idempotence'
  $global:LASTEXITCODE=0
}finally{
  if(!$launcher.HasExited){$launcher.Kill();$launcher.WaitForExit()}
  $launcher.Dispose()
  if($null -ne $owner){$owner.Dispose()}
}
