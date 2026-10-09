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
$outside = Join-Path $root 'outside'
New-Item -ItemType Directory $workspace,$runtime,$state,$outside | Out-Null
foreach ($file in @('latch-boundary-probe.exe','latch-boundary-files.exe','latch-boundary-compat.dll')) {
  Copy-Item -LiteralPath (Join-Path $Binaries $file) -Destination $runtime
}
$runner = Join-Path $runtime 'latch-boundary-probe.exe'
$fixture = Join-Path $runtime 'latch-boundary-files.exe'
$script:failures = 0
function Check([string]$operation,[string]$path,[string]$mode='write',[bool]$mayRefuse=$false) {
  $line = $operation + ' "' + $path + '"'
  & $runner $workspace $fixture $line $mode --read-root $runtime --deny $state
  if ($mayRefuse -and $LASTEXITCODE -eq 125) { Write-Output 'PASS unsafe root refused before execution'; return }
  if ($LASTEXITCODE) { $script:failures++; Write-Output ('FAILED '+$operation+' '+$path) }
}
$plain = Join-Path $outside 'ordinary.txt'
[IO.File]::WriteAllText($plain,'fixture')
Check 'dacl-deny' $plain
Check 'owner-deny' $plain
$nullAcl = Join-Path $outside 'null-dacl.txt'
[IO.File]::WriteAllText($nullAcl,'fixture')
& $fixture 'make-null-dacl' $nullAcl
if ($LASTEXITCODE) { throw 'Could not prepare NULL DACL fixture' }
Check 'write-deny' $nullAcl
$secret = Join-Path $state 'protected-secret.txt'
[IO.File]::WriteAllText($secret,'secret fixture')
& icacls.exe $secret /grant '*S-1-15-2-1:(R)' | Out-Null
$acl = Get-Acl -LiteralPath $secret
$acl.SetAccessRuleProtection($true,$true)
Set-Acl -LiteralPath $secret -AclObject $acl
Check 'read-deny' $secret
$junction = Join-Path $workspace 'junction'
New-Item -ItemType Junction -Path $junction -Target $outside | Out-Null
$junctionAlias = Join-Path $junction 'ordinary.txt'
if ([IO.File]::ReadAllText($junctionAlias) -ne 'fixture') { throw 'Host junction did not resolve to its target' }
$junctionAcl = (Get-Acl -LiteralPath $plain).Sddl
Check 'junction-write-deny' $junctionAlias
if ([IO.File]::ReadAllText($plain) -ne 'fixture' -or (Get-Acl -LiteralPath $plain).Sddl -ne $junctionAcl) {
  throw 'Denied junction write changed its outside target'
}
# Delete only the junction itself, never enumerate or remove its target.
[IO.Directory]::Delete($junction)
$alias = Join-Path $workspace 'hardlink.txt'
New-Item -ItemType HardLink -Path $alias -Target $plain | Out-Null
$before = (Get-Acl -LiteralPath $plain).Sddl
Check 'write-deny' $plain 'write' $true
if ((Get-Acl -LiteralPath $plain).Sddl -ne $before) { throw 'Refused hardlink changed its outside ACL' }
[IO.File]::Delete($alias)
# A read grant must not turn its aliases into writable objects. This was
# previously accepted when read and write roots shared one hardlink allowlist.
New-Item -ItemType HardLink -Path $alias -Target $plain | Out-Null
& $runner $workspace $fixture ('write-allow "'+$alias+'"') write --read-root $runtime --read-root $outside --deny $state
if ($LASTEXITCODE -ne 125) { throw 'Writable alias into a read-only root was not refused' }
if ((Get-Acl -LiteralPath $plain).Sddl -ne $before -or [IO.File]::ReadAllText($plain) -ne 'fixture') {
  throw 'Mixed-permission hardlink changed the outside object'
}
[IO.File]::Delete($alias)
$internal = Join-Path $workspace 'internal.txt'
[IO.File]::WriteAllText($internal,'fixture')
New-Item -ItemType HardLink -Path $alias -Target $internal | Out-Null
Check 'write-allow' $alias
$unicode = Join-Path $workspace (([string][char]0x6D4B)+([string][char]0x8BD5)+' file.txt')
[IO.File]::WriteAllText($unicode,'unicode fixture')
Check 'read-allow' $unicode
Check 'write-allow' $unicode
$protectedWorkspace = Join-Path $workspace 'protected-child.txt'
[IO.File]::WriteAllText($protectedWorkspace,'fixture')
$protectedAcl = Get-Acl -LiteralPath $protectedWorkspace
$protectedAcl.SetAccessRuleProtection($true,$true)
Set-Acl -LiteralPath $protectedWorkspace -AclObject $protectedAcl
$protectedBefore = (Get-Acl -LiteralPath $protectedWorkspace).Sddl
Check 'read-allow' $protectedWorkspace 'read'
Check 'write-allow' $protectedWorkspace
if ((Get-Acl -LiteralPath $protectedWorkspace).Sddl -ne $protectedBefore) {
  throw 'Paired grant did not restore protected workspace ACL exactly'
}
Set-Content -LiteralPath $plain -Stream boundary -Value 'outside stream'
Set-Content -LiteralPath $secret -Stream boundary -Value 'protected stream'
Set-Content -LiteralPath $internal -Stream boundary -Value 'workspace stream'
Check 'write-deny' ($plain+':boundary')
Check 'create-deny' ($plain+':new-stream')
Check 'read-deny' ($secret+':boundary')
Check 'write-allow' ($internal+':boundary')
$replaced=Join-Path $workspace 'replace.txt'
[IO.File]::WriteAllText($replaced,'original contents')
Check 'replace-file' $replaced
if([IO.File]::ReadAllText($replaced) -ne 'replacement contents'){throw 'Replacement data changed during rollback'}
$renamed=Join-Path $workspace 'renamed.txt'
$renameAcl=(Get-Acl -LiteralPath $replaced).Sddl
& $runner $workspace $fixture ('rename-file "'+$replaced+'" "'+$renamed+'"') write --read-root $runtime --deny $state
if($LASTEXITCODE -or (Get-Acl -LiteralPath $renamed).Sddl -ne $renameAcl){throw 'Renamed object ACL was not restored exactly'}
Check 'delete-file' $renamed
if(Test-Path -LiteralPath $renamed){throw 'Rollback recreated a deleted file'}
$gitMarker = Join-Path $workspace '.git'
& $runner $workspace $fixture ('mkdir-deny "'+$gitMarker+'"') write --read-root $runtime --protect-git $workspace
if ($LASTEXITCODE) { throw 'Missing .git metadata was not protected' }
if (Test-Path -LiteralPath $gitMarker) { throw 'Temporary .git reservation survived normal exit' }
Write-Output ('Adversarial failures: '+$script:failures)
if ($script:failures) { exit 1 }
