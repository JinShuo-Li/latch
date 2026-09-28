param(
  [Parameter(Mandatory=$true)][string]$Binaries,
  [Parameter(Mandatory=$true)][string]$FixtureRoot
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$root=[IO.Path]::GetFullPath($FixtureRoot)
if(Test-Path -LiteralPath $root){throw 'Use a new disposable developer fixture'}
$workspace=Join-Path $root 'workspace'
$runtime=Join-Path $root 'runtime'
New-Item -ItemType Directory $workspace,$runtime | Out-Null
foreach($name in @('latch-boundary-probe.exe','latch-boundary-compat.dll','latch-boundary-files.exe')){
  Copy-Item -LiteralPath (Join-Path $Binaries $name) -Destination $runtime
}
$ancestors=@([IO.Path]::GetPathRoot($workspace),[IO.Path]::GetDirectoryName($root))
$ancestorAcls=@{}
foreach($path in $ancestors){$ancestorAcls[$path]=(Get-Acl -LiteralPath $path).Sddl}
$runner=Join-Path $runtime 'latch-boundary-probe.exe'
$env:LATCH_RECOVERY_ROOT=Join-Path $root 'journal'
$shell=Join-Path $env:SystemRoot 'System32/cmd.exe'
$git=(Get-Command git.exe -ErrorAction Stop).Source
$node=(Get-Command node.exe -ErrorAction Stop).Source
$python=(Get-Command python.exe -ErrorAction Stop).Source
$rg=(Get-Command rg.exe -ErrorAction Stop).Source
if ($env:LATCH_TEST_RG_EXE) { $rg=$env:LATCH_TEST_RG_EXE }
if (-not (Test-Path -LiteralPath $rg -PathType Leaf)) { throw "ripgrep executable is missing: $rg" }
function Run([string]$program,[string]$arguments,[string[]]$extra=@()){
  & $runner $workspace $program $arguments write --read-root $runtime --timeout-ms 20000 @extra
  if($LASTEXITCODE){throw ('Sandboxed developer command failed: '+$program+' '+$arguments+' ('+$LASTEXITCODE+')')}
  if(Test-Path -LiteralPath (Join-Path $env:LATCH_RECOVERY_ROOT 'pending')){throw 'Developer command retained its journal'}
}
[IO.File]::WriteAllText((Join-Path $workspace 'input.txt'),'native developer fixture')
[IO.File]::WriteAllText((Join-Path $workspace 'check.js'),'const fs = require("node:fs"); if (!fs.readFileSync("input.txt", "utf8").includes("native")) process.exit(2); fs.writeFileSync("node-output.txt", "node passed");')
[IO.File]::WriteAllText((Join-Path $workspace 'check.py'),'assert "native" in open("input.txt", encoding="utf-8").read(); open("python-output.txt", "w").write("python passed")')
[IO.File]::WriteAllText((Join-Path $workspace 'package.json'),'{"name":"latch-native-fixture","version":"0.0.0","private":true,"scripts":{"test":"node check.js"}}')
Run $node 'check.js'
Run $shell '/d /c dir /a'
Run $shell '/d /c dir /a /s'
Run $shell '/d /c mkdir nested'
[IO.File]::WriteAllText((Join-Path $workspace 'nested/search.txt'),'nested-only-marker')
Run $shell '/d /c echo hello > nested\child.txt'
Run $git 'init --quiet'
Run $git 'symbolic-ref HEAD refs/heads/other'
Run $git 'status --short' @('--deny-write',(Join-Path $workspace '.git'))
Run $git '-C nested init --quiet'
Run $git '-C nested status --short'
Run $git 'add -- input.txt'
Run $git '-c user.name=Native -c user.email=native@example.invalid commit --quiet -m fixture'
Run $git 'worktree add --detach nested-worktree'
Run $git '-C nested-worktree status --short'
# The Chocolatey PATH entry is a launcher shim. Run the actual standalone
# executable from user-owned fixture storage, as production staging does.
$stagedRg=Join-Path $runtime 'rg.exe'
Copy-Item -LiteralPath $rg -Destination $stagedRg
Run $stagedRg 'native input.txt'
Run $stagedRg 'nested-only-marker .'
Run $shell '/d /c type input.txt | findstr native > pipeline.txt && type pipeline.txt'
# Stage user-owned tool runtimes; machine-owned Program Files/ProgramData ACLs
# cannot be modified by an ordinary account. Only the staged files are granted.
$npm=(Get-Command npm.cmd -ErrorAction Stop).Source
Copy-Item -LiteralPath $npm -Destination $runtime
Copy-Item -LiteralPath $node -Destination $runtime
New-Item -ItemType Directory (Join-Path $runtime 'node_modules') | Out-Null
Copy-Item -LiteralPath (Join-Path (Split-Path $npm) 'node_modules/npm') -Destination (Join-Path $runtime 'node_modules') -Recurse
$env:Path=$runtime+';'+$env:Path
$env:npm_config_cache=Join-Path $workspace '.npm-cache'
Run $shell '/d /s /c npm.cmd test --offline'
$pythonHome=Split-Path $python
$versionDll=Get-ChildItem -LiteralPath $pythonHome -Filter 'python3*.dll' -File |
  Where-Object { $_.Name -match '^python3\d+\.dll$' } | Select-Object -First 1
if(-not $versionDll){throw "Python version DLL is missing beside $python"}
Copy-Item -LiteralPath $python -Destination $runtime
foreach($name in @('python3.dll','zlib.dll','ucrtbase.dll')){
  $source=Join-Path $pythonHome $name
  if(Test-Path -LiteralPath $source -PathType Leaf){Copy-Item -LiteralPath $source -Destination $runtime}
}
Copy-Item -LiteralPath $versionDll.FullName -Destination $runtime
Get-ChildItem -LiteralPath $pythonHome -Filter 'vcruntime140*.dll' -File |
  ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $runtime }
New-Item -ItemType Directory (Join-Path $runtime 'Lib') | Out-Null
Copy-Item -LiteralPath (Join-Path $pythonHome 'Lib/encodings') -Destination (Join-Path $runtime 'Lib') -Recurse
Copy-Item -Path (Join-Path $pythonHome 'Lib/*.py') -Destination (Join-Path $runtime 'Lib')
$pthName=[IO.Path]::ChangeExtension($versionDll.Name,'._pth')
[IO.File]::WriteAllText((Join-Path $runtime $pthName),"Lib`r`n.`r`n")
Run (Join-Path $runtime 'python.exe') '-I -S check.py'
foreach($name in @('pipeline.txt','node-output.txt','python-output.txt')){
  if(!(Test-Path -LiteralPath (Join-Path $workspace $name))){throw ('Developer output missing: '+$name)}
}
foreach($path in $ancestors){
  if((Get-Acl -LiteralPath $path).Sddl -ne $ancestorAcls[$path]){throw ('Ancestor ACL changed: '+$path)}
}
Write-Output 'PASS native Git/nested repo/worktree, ripgrep, cmd pipeline/redirect, Node/npm and Python workflows'
$global:LASTEXITCODE=0
