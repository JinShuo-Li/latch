param([Parameter(Mandatory=$true)][string]$Binaries,[Parameter(Mandatory=$true)][string]$FixtureRoot)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$cancelOnly=$env:LATCH_RECOVERY_CANCEL_ONLY -eq '1'
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
$points=@('after-journal','profile-intent','profile-api-key','profile-api-directory','profile-api-mapping','profile-orphan-conflict','profile-unsealed','profile-resealed','profile-key-conflict','profile-mapping-conflict','first-acl','all-grants','appcontainer','child-launch','descendants','cleanup','rollback-sealed','profile-removed','reboot-equivalent','torn-intent','corrupt-record','host-acl-conflict','replaced-object')
if($env:LATCH_RECOVERY_POINTS){$points=$env:LATCH_RECOVERY_POINTS.Split(',')}
foreach($point in $points) {
  $case=Join-Path $root $point;$workspace=Join-Path $case 'workspace';$runtime=Join-Path $case 'runtime';$state=Join-Path $case 'state';$journal=Join-Path $case 'journal'
  New-Item -ItemType Directory $case,$workspace,$runtime,$state | Out-Null
  foreach($file in @('latch-boundary-probe.exe','latch-boundary-files.exe','latch-boundary-compat.dll')){Copy-Item -LiteralPath (Join-Path $Binaries $file) -Destination $runtime}
  $ordinary=Join-Path $workspace 'ordinary.txt';$secret=Join-Path $state 'secret.txt'
  [IO.File]::WriteAllText($ordinary,'workspace');[IO.File]::WriteAllText($secret,'synthetic credential')
  if($point -eq 'large-tree'){
    $deep=Join-Path $workspace 'one/two/three/four/five/six'
    [IO.Directory]::CreateDirectory($deep) | Out-Null
    foreach($number in 1..1000){[IO.File]::WriteAllText((Join-Path $deep ($number.ToString()+'.txt')),'fixture')}
  }
  & icacls.exe $secret /grant '*S-1-15-2-1:(R)' | Out-Null
  if($LASTEXITCODE){throw 'Could not prepare credential ACL'}
  $acl=Get-Acl -LiteralPath $secret;$acl.SetAccessRuleProtection($true,$true);Set-Acl -LiteralPath $secret -AclObject $acl
  $before=@{}
  foreach($path in @($workspace,$ordinary,$runtime,$state,$secret)+(Get-ChildItem -LiteralPath $runtime -File | ForEach-Object FullName)){ $before[$path]=(Get-Acl -LiteralPath $path).Sddl }
  $runner=Join-Path $runtime 'latch-boundary-probe.exe';$fixture=Join-Path $runtime 'latch-boundary-files.exe';$marker=Join-Path $workspace 'tree'
  $pause=if($point -in @('host-acl-conflict','replaced-object','torn-intent','corrupt-record')){'all-grants'}elseif($point -in @('profile-mapping-conflict','profile-resealed')){'profile-unsealed'}elseif($point -eq 'profile-orphan-conflict'){'profile-intent'}elseif($point -eq 'profile-key-conflict'){'profile-api-key'}elseif($point -eq 'reboot-equivalent'){'descendants'}else{$point}
  if($point -eq 'large-tree'){$pause='cleanup'}
  $command=if($point -in @('descendants','reboot-equivalent')){'tree 3 '+$marker}else{'read-allow '+$ordinary}
  $info=[Diagnostics.ProcessStartInfo]::new();$info.FileName=$runner;$info.UseShellExecute=$false;$info.CreateNoWindow=$true;$info.RedirectStandardError=$true;$q=[char]34
  $info.Arguments=$q+$workspace+$q+' '+$q+$fixture+$q+' '+$q+$command+$q+' write --read-root '+$q+$runtime+$q+' --deny '+$q+$state+$q+' --protect-git '+$q+$workspace+$q
  $info.EnvironmentVariables['LATCH_RECOVERY_ROOT']=$journal;$info.EnvironmentVariables['LATCH_RECOVERY_PAUSE']=$pause
  $launcher=[Diagnostics.Process]::Start($info);$owner=$null;$children=@()
  try {
    $deadline=[DateTime]::UtcNow.AddSeconds(20);$signal=Join-Path $journal 'pause.pid'
    while(!(Test-Path -LiteralPath $signal)){if($launcher.HasExited){$reported=$launcher.StandardError.ReadToEndAsync();$null=$reported.Wait(2000);throw ('Crash checkpoint not reached: '+$point+' exit='+$launcher.ExitCode+' stderr='+$(if($reported.IsCompleted){$reported.Result}else{'pending'}))};if([DateTime]::UtcNow -gt $deadline){throw ('Crash checkpoint timed out: '+$point)};Start-Sleep -Milliseconds 20}
    $ownerId=[BitConverter]::ToInt32([IO.File]::ReadAllBytes($signal),0);$owner=[Diagnostics.Process]::GetProcessById($ownerId);$header=Header $journal
    if($point -eq 'large-tree'){
      $records=@(Get-ChildItem -LiteralPath (Join-Path $journal 'pending') -Filter '*.rec').Count
      if($owner.HandleCount -gt ($records+1000)){throw ('Rollback retained duplicate ancestor handles: '+$owner.HandleCount)}
      Write-Output ('PASS large-tree bounded handle count: '+$owner.HandleCount+' for '+$records+' records')
    }
    if($point -in @('descendants','reboot-equivalent')){
      while(!(Test-Path -LiteralPath ($marker+'.0'))){if([DateTime]::UtcNow -gt $deadline){throw 'Four generations did not start'};Start-Sleep -Milliseconds 20}
      foreach($generation in 0..3){$childId=[BitConverter]::ToInt32([IO.File]::ReadAllBytes($marker+'.'+$generation),0);$children += [Diagnostics.Process]::GetProcessById($childId)}
    } elseif($point -eq 'child-launch'){
      foreach($child in @(Get-CimInstance Win32_Process -Filter ('ParentProcessId='+$ownerId))){$children += [Diagnostics.Process]::GetProcessById([int]$child.ProcessId)}
      if($children.Count -lt 1){throw 'Suspended child not found'}
    }
    if($cancelOnly){
      $launcher.Kill()
      [IO.File]::WriteAllText((Join-Path $journal 'pause.resume'),'release fixture checkpoint')
      if(!$owner.WaitForExit(20000)){throw ('Owner did not clean up launcher cancellation: '+$point)}
    }else{
      if($point -eq 'reboot-equivalent'){$launcher.Kill()}
      $owner.Kill();if(!$owner.WaitForExit(5000)){throw 'Cleanup owner survived forced termination'}
    }
    if(!$launcher.WaitForExit(5000)){throw 'Launcher survived owner death'}
    foreach($child in $children){if(!$child.WaitForExit(5000)){throw ('Descendant survived cleanup-owner death: '+$child.Id)}}
    if(!$cancelOnly -and !(Test-Path -LiteralPath (Join-Path $journal 'pending'))){throw 'Crash lost its durable journal'}
    if($cancelOnly -and (Test-Path -LiteralPath (Join-Path $journal 'pending'))){throw 'Cancellation left a pending transaction'}
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
    if($point -eq 'profile-resealed'){
      Remove-Item -LiteralPath $signal
      $info.Arguments='--recover-only'
      $info.EnvironmentVariables['LATCH_RECOVERY_PAUSE']='profile-resealed'
      $repair=[Diagnostics.Process]::Start($info)
      try {
        $deadline=[DateTime]::UtcNow.AddSeconds(20)
        while(!(Test-Path -LiteralPath $signal)){if($repair.HasExited -or [DateTime]::UtcNow -gt $deadline){throw 'Recovery reseal checkpoint not reached'};Start-Sleep -Milliseconds 20}
        $repair.Kill();if(!$repair.WaitForExit(5000)){throw 'Recovery process survived kill'}
      } finally {if(!$repair.HasExited){$repair.Kill();$repair.WaitForExit()};$repair.Dispose()}
    }
    if($point -eq 'profile-key-conflict'){
      $mapping='HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings\'+$header[4]
      try {
        Set-ItemProperty -LiteralPath $mapping -Name Foreign -Value 'preserve'
        & $runner --recover-only
        if($LASTEXITCODE -ne 125 -or !(Test-Path -LiteralPath (Join-Path $journal 'pending'))){throw 'Populated incomplete mapping did not retain journal'}
        if((Get-ItemProperty -LiteralPath $mapping -Name Foreign).Foreign -ne 'preserve'){throw 'Recovery modified foreign mapping value'}
      } finally {Remove-ItemProperty -LiteralPath $mapping -Name Foreign}
    }
    if($point -eq 'profile-orphan-conflict'){
      New-Item -ItemType Directory -Path $header[7] | Out-Null
      $orphan=Join-Path $header[7] 'sentinel.txt'
      [IO.File]::WriteAllText($orphan,'unrelated directory')
      try {
        & $runner --recover-only
        if($LASTEXITCODE -ne 125 -or !(Test-Path -LiteralPath (Join-Path $journal 'pending'))){throw 'Unregistered directory did not retain journal'}
        if([IO.File]::ReadAllText($orphan) -ne 'unrelated directory'){throw 'Unregistered directory was changed'}
      } finally {
        Remove-Item -LiteralPath $orphan
        Remove-Item -LiteralPath $header[7]
      }
    }
    if($point -eq 'profile-mapping-conflict'){
      $mapping='HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings\'+$header[4]
      if(!(Test-Path -LiteralPath $mapping)){throw 'Unsealed profile has no registration'}
      $moniker=(Get-ItemProperty -LiteralPath $mapping -Name Moniker).Moniker
      if($moniker -ne $header[3]){throw 'Unexpected profile moniker'}
      try {
        Set-ItemProperty -LiteralPath $mapping -Name Moniker -Value 'unrelated-profile'
        & $runner --recover-only
        if($LASTEXITCODE -ne 125 -or !(Test-Path -LiteralPath (Join-Path $journal 'pending'))){throw 'Mismatched mapping did not retain journal'}
        if(!(Test-Path -LiteralPath $header[7])){throw 'Mismatched mapping deleted package'}
      } finally {Set-ItemProperty -LiteralPath $mapping -Name Moniker -Value $moniker}
    }
    if($point -eq 'reboot-equivalent'){
      # Normal runner startup must recover stale state before launching again.
      & $runner $workspace $fixture ('read-allow '+$ordinary) write --read-root $runtime --deny $state --protect-git $workspace
    }else{& $runner --recover-only}
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
