param(
  [Parameter(Mandatory=$true)][string]$Binaries,
  [Parameter(Mandatory=$true)][string]$FixtureRoot,
  [Parameter(Mandatory=$true)][string]$Toolchain
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$root=[IO.Path]::GetFullPath($FixtureRoot)
$toolchainRoot=[IO.Path]::GetFullPath($Toolchain)
if(Test-Path -LiteralPath $root){throw 'Use a new disposable Cargo fixture'}
if(!$env:LIB){throw 'Use an x64 Visual Studio developer shell'}
$workspace=Join-Path $root 'workspace';$runtime=Join-Path $root 'runtime'
New-Item -ItemType Directory $root,$workspace,$runtime,(Join-Path $workspace 'src') | Out-Null
foreach($file in @('latch-boundary-probe.exe','latch-boundary-compat.dll')){Copy-Item -LiteralPath (Join-Path $Binaries $file) -Destination $runtime}
$manifest=@('[package]','name = "latch-recovery-cargo-fixture"','version = "0.0.0"','edition = "2024"')
$manifest | Set-Content -LiteralPath (Join-Path $workspace 'Cargo.toml')
$source=@('/// ```','/// assert_eq!(latch_recovery_cargo_fixture::add(2, 3), 5);','/// ```','pub fn add(a: usize, b: usize) -> usize { a + b }','#[cfg(test)]','mod tests {','    #[test] fn addition() { assert_eq!(super::add(20, 22), 42); }','}')
$source | Set-Content -LiteralPath (Join-Path $workspace 'src/lib.rs')
$saved=@{}
foreach($key in @('PATH','TEMP','TMP','CARGO_HOME','CARGO_TARGET_DIR')){$saved[$key]=[Environment]::GetEnvironmentVariable($key)}
try {
  $env:PATH=(Join-Path $toolchainRoot 'bin')+';'+$env:PATH
  $env:CARGO_HOME=Join-Path $workspace '.cargo'
  $env:CARGO_TARGET_DIR=Join-Path $workspace 'target'
  $env:TEMP=$workspace;$env:TMP=$workspace
  # Executable/library roots suffice; do not grant thousands of HTML manuals.
  & (Join-Path $runtime 'latch-boundary-probe.exe') $workspace (Join-Path $toolchainRoot 'bin/cargo.exe') 'test --offline' write --read-root $runtime --read-root (Join-Path $toolchainRoot 'bin') --read-root (Join-Path $toolchainRoot 'lib')
  if($LASTEXITCODE){throw ('Sandboxed Cargo/cleanup failed: '+$LASTEXITCODE)}
  Write-Output 'PASS fresh sandboxed Cargo compile/link/unit/rustdoc tests and cleanup'
} finally {
  foreach($key in $saved.Keys){[Environment]::SetEnvironmentVariable($key,$saved[$key])}
}
$global:LASTEXITCODE=0
