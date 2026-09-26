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
Check 'write-deny' (Join-Path $junction 'ordinary.txt')
# Delete only the junction itself, never enumerate or remove its target.
[IO.Directory]::Delete($junction)
$alias = Join-Path $workspace 'hardlink.txt'
New-Item -ItemType HardLink -Path $alias -Target $plain | Out-Null
$before = (Get-Acl -LiteralPath $plain).Sddl
Check 'write-deny' $plain 'write' $true
if ((Get-Acl -LiteralPath $plain).Sddl -ne $before) { throw 'Refused hardlink changed its outside ACL' }
[IO.File]::Delete($alias)
$internal = Join-Path $workspace 'internal.txt'
[IO.File]::WriteAllText($internal,'fixture')
New-Item -ItemType HardLink -Path $alias -Target $internal | Out-Null
Check 'write-allow' $alias
$gitMarker = Join-Path $workspace '.git'
& $runner $workspace $fixture ('mkdir-deny "'+$gitMarker+'"') write --read-root $runtime --protect-git $workspace
if ($LASTEXITCODE) { throw 'Missing .git metadata was not protected' }
if (Test-Path -LiteralPath $gitMarker) { throw 'Temporary .git reservation survived normal exit' }
Write-Output ('Adversarial failures: '+$script:failures)
if ($script:failures) { exit 1 }
