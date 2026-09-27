param([Parameter(Mandatory=$true)][string]$Binaries,[Parameter(Mandatory=$true)][string]$FixtureRoot)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$root=[IO.Path]::GetFullPath($FixtureRoot)
if(Test-Path -LiteralPath $root){throw 'Use a new disposable recovery fixture'}
New-Item -ItemType Directory $root | Out-Null
function Header([string]$journal){
  $reader=[IO.BinaryReader]::new([IO.MemoryStream]::new([IO.File]::ReadAllBytes((Join-Path $journal 'pending/00000000.rec'))))
  try {
    $null=$reader.ReadUInt32();$null=$reader.ReadUInt32();$null=$reader.ReadUInt32();$count=$reader.ReadUInt32()
    $fields=@(); for($i=0;$i -lt $count;$i++){ $length=$reader.ReadUInt32();$fields += [Text.Encoding]::Unicode.GetString($reader.ReadBytes($length*2)) }; return ,$fields
  } finally {$reader.Dispose()}
}
foreach($point in @('after-journal','profile-intent','profile-unsealed','first-acl','all-grants','appcontainer','child-launch','descendants','cleanup','rollback-sealed','profile-removed','reboot-equivalent','torn-intent','corrupt-record','host-acl-conflict','replaced-object')) {
  $case=Join-Path $root $point;$workspace=Join-Path $case 'workspace';$runtime=Join-Path $case 'runtime';$state=Join-Path $case 'state';$journal=Join-Path $case 'journal'
  New-Item -ItemType Directory $case,$workspace,$runtime,$state | Out-Null
  foreach($file in @('latch-boundary-probe.exe','latch-boundary-files.exe','latch-boundary-compat.dll')){Copy-Item -LiteralPath (Join-Path $Binaries $file) -Destination $runtime}
  $ordinary=Join-Path $workspace 'ordinary.txt';$secret=Join-Path $state 'secret.txt'
  [IO.File]::WriteAllText($ordinary,'workspace');[IO.File]::WriteAllText($secret,'synthetic credential')
  & icacls.exe $secret /grant '*S-1-15-2-1:(R)' | Out-Null
  if($LASTEXITCODE){throw 'Could not prepare credential ACL'}
  $acl=Get-Acl -LiteralPath $secret;$acl.SetAccessRuleProtection($true,$true);Set-Acl -LiteralPath $secret -AclObject $acl
  $before=@{}
  foreach($path in @($workspace,$ordinary,$runtime,$state,$secret)+(Get-ChildItem -LiteralPath $runtime -File | ForEach-Object FullName)){ $before[$path]=(Get-Acl -LiteralPath $path).Sddl }
  $runner=Join-Path $runtime 'latch-boundary-probe.exe';$fixture=Join-Path $runtime 'latch-boundary-files.exe';$marker=Join-Path $workspace 'tree'
  $pause=if($point -in @('host-acl-conflict','replaced-object','torn-intent','corrupt-record')){'all-grants'}elseif($point -eq 'reboot-equivalent'){'descendants'}else{$point}
  $command=if($point -in @('descendants','reboot-equivalent')){'tree 3 '+$marker}else{'read-allow '+$ordinary}
  $info=[Diagnostics.ProcessStartInfo]::new();$info.FileName=$runner;$info.UseShellExecute=$false;$info.CreateNoWindow=$true;$q=[char]34
  $info.Arguments=$q+$workspace+$q+' '+$q+$fixture+$q+' '+$q+$command+$q+' write --read-root '+$q+$runtime+$q+' --deny '+$q+$state+$q+' --protect-git '+$q+$workspace+$q
  $info.EnvironmentVariables['LATCH_RECOVERY_ROOT']=$journal;$info.EnvironmentVariables['LATCH_RECOVERY_PAUSE']=$pause
  $launcher=[Diagnostics.Process]::Start($info);$owner=$null;$children=@()
  try {
    $deadline=[DateTime]::UtcNow.AddSeconds(20);$signal=Join-Path $journal 'pause.pid'
    while(!(Test-Path -LiteralPath $signal)){if($launcher.HasExited -or [DateTime]::UtcNow -gt $deadline){throw ('Crash checkpoint not reached: '+$point)};Start-Sleep -Milliseconds 20}
    $ownerId=[BitConverter]::ToInt32([IO.File]::ReadAllBytes($signal),0);$owner=[Diagnostics.Process]::GetProcessById($ownerId);$header=Header $journal
    if($point -in @('descendants','reboot-equivalent')){
      while(!(Test-Path -LiteralPath ($marker+'.0'))){if([DateTime]::UtcNow -gt $deadline){throw 'Four generations did not start'};Start-Sleep -Milliseconds 20}
      foreach($generation in 0..3){$childId=[BitConverter]::ToInt32([IO.File]::ReadAllBytes($marker+'.'+$generation),0);$children += [Diagnostics.Process]::GetProcessById($childId)}
    } elseif($point -eq 'child-launch'){
      foreach($child in @(Get-CimInstance Win32_Process -Filter ('ParentProcessId='+$ownerId))){$children += [Diagnostics.Process]::GetProcessById([int]$child.ProcessId)}
      if($children.Count -lt 1){throw 'Suspended child not found'}
    }
    if($point -eq 'reboot-equivalent'){$launcher.Kill()}
    $owner.Kill();if(!$owner.WaitForExit(5000)){throw 'Cleanup owner survived forced termination'}
    if(!$launcher.WaitForExit(5000)){throw 'Launcher survived owner death'}
    foreach($child in $children){if(!$child.WaitForExit(5000)){throw ('Descendant survived cleanup-owner death: '+$child.Id)}}
    if(!(Test-Path -LiteralPath (Join-Path $journal 'pending'))){throw 'Crash lost its durable journal'}
    if($point -eq 'host-acl-conflict'){
      $acl=Get-Acl -LiteralPath $ordinary
      $rule=[Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-5-7'),'Read','Allow')
      $acl.AddAccessRule($rule);Set-Acl -LiteralPath $ordinary -AclObject $acl
      $hostAcl=(Get-Acl -LiteralPath $ordinary).Sddl
    } elseif($point -eq 'replaced-object'){
      Move-Item -LiteralPath $ordinary -Destination ($ordinary+'.original')
      [IO.File]::WriteAllText($ordinary,'replacement must remain untouched');$hostAcl=(Get-Acl -LiteralPath $ordinary).Sddl
    }
    if($point -eq 'torn-intent'){[IO.File]::WriteAllBytes((Join-Path $journal 'pending/99999999.rec.tmp'),[byte[]](1,2,3))}
    if($point -eq 'corrupt-record'){
      $file=Join-Path $journal 'pending/00000001.rec';$bytes=[IO.File]::ReadAllBytes($file);$bytes[8]=$bytes[8] -bxor 1;[IO.File]::WriteAllBytes($file,$bytes)
      $hostAcl=(Get-Acl -LiteralPath $ordinary).Sddl
    }
    $env:LATCH_RECOVERY_ROOT=$journal;Remove-Item Env:LATCH_RECOVERY_PAUSE -ErrorAction SilentlyContinue
    if($point -eq 'reboot-equivalent'){
      # Normal runner startup must recover stale state before launching again.
      & $runner $workspace $fixture ('read-allow '+$ordinary) write --read-root $runtime --deny $state --protect-git $workspace
    }else{& $runner --recover-only}
    if($point -eq 'profile-unsealed'){
      if($LASTEXITCODE -ne 125){throw 'Unsealed API mutation was not blocked'}
      if(!(Test-Path -LiteralPath (Join-Path $journal 'pending'))){throw 'Unsealed journal was lost'}
      foreach($path in $before.Keys){if((Get-Acl -LiteralPath $path).Sddl -ne $before[$path]){throw 'Unsealed creation changed host ACLs'}}
      Write-Output 'PASS unsealed profile creation fails closed and requires operator reconciliation';continue
    }
    if($point -in @('host-acl-conflict','replaced-object','corrupt-record')){
      if($LASTEXITCODE -ne 125){throw 'Host conflict did not fail closed'}
      if((Get-Acl -LiteralPath $ordinary).Sddl -ne $hostAcl){throw 'Recovery overwrote an unrelated host change'}
      if(!(Test-Path -LiteralPath (Join-Path $journal 'pending'))){throw 'Conflicted journal was discarded'}
      Write-Output ('PASS '+$point+' preserved host state and retained journal');continue
    }
    if($LASTEXITCODE){throw ('Recovery failed: '+$point)}
    & $runner --recover-only;if($LASTEXITCODE){throw 'Second recovery failed'}
    foreach($path in $before.Keys){if((Get-Acl -LiteralPath $path).Sddl -ne $before[$path]){throw ('ACL was not restored exactly: '+$path)}}
    if(Test-Path -LiteralPath (Join-Path $workspace '.git')){throw 'Git reservation survived recovery'}
    if(Test-Path -LiteralPath $header[7]){throw 'AppContainer package survived recovery'}
    $mapping='HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings\'+$header[4]
    if(Test-Path -LiteralPath $mapping){throw 'AppContainer profile survived recovery'}
    if(Test-Path -LiteralPath (Join-Path $journal 'pending')){throw 'Completed journal was not removed'}
    Write-Output ('PASS '+$point+' exact ACL/profile/git rollback and idempotent recovery')
  } finally {
    if($owner){if(!$owner.HasExited){$owner.Kill();$owner.WaitForExit()};$owner.Dispose()}
    if(!$launcher.HasExited){$launcher.Kill();$launcher.WaitForExit()};$launcher.Dispose()
    foreach($child in $children){$child.Dispose()}
    Remove-Item Env:LATCH_RECOVERY_ROOT -ErrorAction SilentlyContinue
  }
}

$global:LASTEXITCODE=0
