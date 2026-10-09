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
$nested = Join-Path $workspace 'nested'
$sensitive = Join-Path $workspace '.latch'
New-Item -ItemType Directory $workspace,$runtime,$nested,$sensitive | Out-Null
foreach ($file in @('latch-boundary-probe.exe','latch-boundary-files.exe','latch-boundary-compat.dll')) {
  Copy-Item -LiteralPath (Join-Path $Binaries $file) -Destination $runtime
}
$runner = Join-Path $runtime 'latch-boundary-probe.exe'
$fixture = Join-Path $runtime 'latch-boundary-files.exe'
$ordinary = Join-Path $nested 'readable.txt'
$outside = Join-Path $root 'outside.txt'
$runtimeData = Join-Path $runtime 'data'
$runtimeAsset = Join-Path $runtimeData 'asset.txt'
New-Item -ItemType Directory $runtimeData | Out-Null
[IO.File]::WriteAllText($runtimeAsset, 'read-only runtime asset')
[IO.File]::WriteAllText($ordinary, 'readable without WRITE_DAC')
[IO.File]::WriteAllText($outside, 'outside private')
$secret = Join-Path $sensitive 'secret.txt'
[IO.File]::WriteAllText($secret, 'synthetic secret')
$alias = Join-Path $workspace 'secret-alias.txt'
New-Item -ItemType HardLink -Path $alias -Target $secret | Out-Null
$git = (Get-Command git.exe -ErrorAction Stop).Source
& $git init --quiet $workspace
if ($LASTEXITCODE) { throw 'Cannot initialize Git fixture' }
$objects = @($runtimeData,$runtimeAsset,$workspace,$nested,$ordinary,$sensitive,$secret,(Join-Path $workspace '.git'),(Join-Path $workspace '.git/config'))
# Keep host recovery handles before removing WRITE_DAC. OWNER RIGHTS disables
# the owner's implicit WRITE_DAC, reproducing a readable foreign-owned tree
# without changing ownership or requiring elevation. Restore the exact DACL
# through those existing handles in finally, even if the fixture fails.
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class LatchWriteAclFixture {
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  public static extern IntPtr CreateFileW(string path, uint access, uint share,
      IntPtr security, uint disposition, uint flags, IntPtr template);
  [DllImport("advapi32.dll", SetLastError=true)]
  public static extern bool SetKernelObjectSecurity(IntPtr handle, uint parts, byte[] descriptor);
  [DllImport("kernel32.dll")]
  public static extern bool CloseHandle(IntPtr handle);
}
'@
$restore = @()
try {
  $user = [Security.Principal.WindowsIdentity]::GetCurrent().User
  $ownerRights = [Security.Principal.SecurityIdentifier]::new('S-1-3-4')
  foreach ($path in $objects) {
    $acl = Get-Acl -LiteralPath $path
    $handle = [LatchWriteAclFixture]::CreateFileW($path, 0x40000, 7, [IntPtr]::Zero, 3, 0x02000000, [IntPtr]::Zero)
    if ($handle -eq [IntPtr]::new(-1)) { throw "Cannot pin fixture restore handle: $path" }
    $restore += @{ Handle=$handle; Descriptor=$acl.GetSecurityDescriptorBinaryForm(); Path=$path }
  }
  foreach ($item in $restore) {
    $path = $item.Path
    $acl = Get-Acl -LiteralPath $path
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($rule in @($acl.Access)) { [void]$acl.RemoveAccessRuleSpecific($rule) }
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
      $user, [Security.AccessControl.FileSystemRights]::Modify,
      [Security.AccessControl.AccessControlType]::Allow))
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
      $ownerRights, [Security.AccessControl.FileSystemRights]::ReadPermissions,
      [Security.AccessControl.AccessControlType]::Allow))
    if (![LatchWriteAclFixture]::SetKernelObjectSecurity($item.Handle, 4, $acl.GetSecurityDescriptorBinaryForm())) {
      throw "Cannot restrict synthetic fixture DACL: $path"
    }
    $denied = [LatchWriteAclFixture]::CreateFileW($path, 0x40000, 7, [IntPtr]::Zero, 3, 0x02000000, [IntPtr]::Zero)
    if ($denied -ne [IntPtr]::new(-1)) {
      [void][LatchWriteAclFixture]::CloseHandle($denied)
      throw "Fixture still permits WRITE_DAC: $path"
    }
  }
  $before = @{}
  foreach ($path in $objects) { $before[$path]=(Get-Acl -LiteralPath $path).Sddl }
  foreach ($case in @(
    @('enumerate-allow',$workspace), @('enumerate-allow',$nested),
    @('read-allow',$ordinary), @('write-allow',$ordinary), @('create-allow',(Join-Path $nested 'created.txt')), @('read-allow',$runtimeAsset), @('crt-read',$ordinary), 
    @('read-deny',$outside), @('enumerate-deny',$root),
    @('read-deny',$secret), @('read-deny',$alias), @('enumerate-deny',$sensitive),
    @('dacl-deny',$ordinary), @('owner-deny',$ordinary), @('create-existing',$ordinary),
    @('broker-read-allow',$ordinary), @('broker-read-deny',$secret),
    @('raw-directory-write-deny',$workspace), @('raw-namespace-deny',$workspace), @('raw-dacl-deny',$ordinary),
    @('broker-access-deny',$ordinary), @('broker-caller-deny',$ordinary)
  )) {
    & $runner $workspace $fixture ($case[0]+' "'+$case[1]+'"') write --filesystem broker --read-root $runtime --deny $sensitive --protect-git $workspace --timeout-ms 10000
    if ($LASTEXITCODE) { throw "Read broker assertion failed: $($case[0])" }
  }
  foreach($target in @($outside,$secret,$alias,$runtimeAsset,(Join-Path $workspace '.git/config'))) {
    & $runner $workspace $fixture ('write-deny "'+$target+'"') write --filesystem broker --read-root $runtime --deny $sensitive --deny-write $runtime --protect-git $workspace --timeout-ms 10000
    if($LASTEXITCODE){throw "Mediated write escaped scope: $target"}
  }
  $cmd = Join-Path $env:SystemRoot 'System32/cmd.exe'
  foreach ($command in @('/d /c dir /a /s', '/d /c cmd /d /c dir /b nested')) {
    # Recursive dir reports access denied for the deliberately masked subtree.
    $text = & $runner $workspace $cmd $command write --filesystem broker --read-root $runtime --deny $sensitive --protect-git $workspace --timeout-ms 10000
    if ($LASTEXITCODE -or ($text -join "`n") -notmatch 'readable') { throw "Native shell broker failed: $command" }
  }
  foreach($operation in @('raw-rename-outside-deny','raw-link-outside-deny')) {
    & $runner $workspace $fixture ($operation+' "'+$ordinary+'" "'+$root+'"') write --filesystem broker --read-root $runtime --deny $sensitive --protect-git $workspace --timeout-ms 10000
    if($LASTEXITCODE){throw "Raw native namespace operation escaped: $operation"}
  }
  foreach($command in @('/d /c move /y nested\created.txt nested\renamed.txt','/d /c del nested\renamed.txt','/d /c mkdir generated','/d /c rmdir generated')) {
    & $runner $workspace $cmd $command write --filesystem broker --read-root $runtime --deny $sensitive --protect-git $workspace --timeout-ms 10000
    if($LASTEXITCODE){throw "Mediated native mutation failed: $command"}
  }
  $status = & $runner $workspace $git 'status --short' write --filesystem broker --read-root $runtime --deny $sensitive --protect-git $workspace --timeout-ms 10000
  if ($LASTEXITCODE -or ($status -join "`n") -notmatch 'nested') { throw 'Read broker Git status failed' }
  foreach ($path in $before.Keys) {
    if ((Get-Acl -LiteralPath $path).Sddl -ne $before[$path]) { throw "Read broker changed source ACL: $path" }
  }
  Write-Output 'PASS mediated writes and reads without WRITE_DAC or source ACL changes'
} finally {
  foreach ($item in $restore) {
    try {
      if (![LatchWriteAclFixture]::SetKernelObjectSecurity($item.Handle, 4, $item.Descriptor)) {
        throw "Cannot restore synthetic fixture DACL: $($item.Path)"
      }
    } finally { [void][LatchWriteAclFixture]::CloseHandle($item.Handle) }
  }
}
$global:LASTEXITCODE = 0
