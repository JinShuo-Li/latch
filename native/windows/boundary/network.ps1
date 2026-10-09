param(
  [Parameter(Mandatory=$true)][string]$Binaries,
  [Parameter(Mandatory=$true)][string]$FixtureRoot,
  [string]$DnsName = 'example.com',
  [string]$PrivateLanAddress,
  [ValidateRange(0,65535)][int]$PrivateLanPort = 0
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = [IO.Path]::GetFullPath($FixtureRoot)
if (Test-Path -LiteralPath $root) { throw 'Use a new disposable fixture directory' }
$workspace = Join-Path $root 'workspace'
$runtime = Join-Path $root 'runtime'
New-Item -ItemType Directory -Path $root,$workspace,$runtime | Out-Null
foreach ($file in @('latch-boundary-probe.exe','latch-boundary-files.exe','latch-boundary-compat.dll')) {
  Copy-Item -LiteralPath (Join-Path $Binaries $file) -Destination $runtime
}
$runner = Join-Path $runtime 'latch-boundary-probe.exe'
$fixture = Join-Path $runtime 'latch-boundary-files.exe'
$template = Join-Path $workspace 'baseline-template.txt'
[IO.File]::WriteAllText($template, 'baseline')
$templateAcl = (Get-Acl -LiteralPath $template).Sddl
$baseline = (Get-Acl -LiteralPath $workspace).Sddl
$q = [char]34

function Invoke-SandboxClientProbe([string]$Operation, [int]$Port,
                                   [string]$HostName, [string]$Network) {
  $command = "$Operation $Port $HostName"
  $escaped = $command.Replace('"','\"')
  & $runner $workspace $fixture $escaped read --read-root $runtime --network $Network
  if ($LASTEXITCODE) {
    throw "Sandbox network policy mismatch: $command network=$Network exit=$LASTEXITCODE"
  }
}

function Start-SandboxRunner([string]$Command, [string]$Mode,
                             [string]$Network, [int]$TimeoutMs, [string]$Program=$fixture) {
  $info = New-Object Diagnostics.ProcessStartInfo
  $info.FileName = $runner
  $info.UseShellExecute = $false
  $info.CreateNoWindow = $true
  $escaped = $Command.Replace('"','\"')
  $info.Arguments = $q+$workspace+$q+' '+$q+$Program+$q+' '+$q+$escaped+$q+
    ' '+$Mode+' --read-root '+$q+$runtime+$q+' --network '+$Network+
    ' --timeout-ms '+$TimeoutMs
  return [Diagnostics.Process]::Start($info)
}

function Test-HostTcp([Net.IPAddress]$Address, [int]$Port) {
  $family = if ($Address.AddressFamily -eq [Net.Sockets.AddressFamily]::InterNetworkV6) {
    [Net.Sockets.AddressFamily]::InterNetworkV6
  } else {
    [Net.Sockets.AddressFamily]::InterNetwork
  }
  $client = [Net.Sockets.TcpClient]::new($family)
  try {
    $pending = $client.BeginConnect($Address, $Port, $null, $null)
    if (!$pending.AsyncWaitHandle.WaitOne(3000)) { return $false }
    $client.EndConnect($pending)
    return $true
  } catch {
    return $false
  } finally {
    $client.Close()
  }
}

# DNS uses a separate native resolver probe. Skip the external-name assertion
# only when the host itself cannot resolve the configured name.
if ($DnsName -notmatch '^[A-Za-z0-9.-]+$') { throw 'DnsName must be a DNS hostname' }
$dnsBaseline = @()
try { $dnsBaseline = @([Net.Dns]::GetHostAddresses($DnsName)) } catch { }
if ($dnsBaseline.Count) {
  & $runner $workspace $fixture ('network-resolve '+$DnsName) read --read-root $runtime --network yes
  if ($LASTEXITCODE) { throw "Sandbox could not resolve $DnsName with network capability" }
  Write-Output "PASS DNS resolution for $DnsName with network capability"
} else {
  Write-Output "SKIP external DNS: host could not resolve $DnsName"
}

# localhost forces the native probe through GetAddrInfoW (DNS/name resolution)
# and tries both returned families. The numeric case checks IPv4 directly.
foreach ($network in @('no','yes')) {
  $udpHostListener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
  $udpHostListener.Start()
  try {
    $port = ([Net.IPEndPoint]$udpHostListener.LocalEndpoint).Port
    Invoke-SandboxClientProbe $(if ($network -eq 'yes') {'network-allow'} else {'network-deny'}) $port 'localhost' $network
    Invoke-SandboxClientProbe $(if ($network -eq 'yes') {'network-allow'} else {'network-deny'}) $port '127.0.0.1' $network
  } finally {
    $udpHostListener.Stop()
  }
}
Write-Output 'PASS localhost resolution and IPv4 loopback client policy with and without network capability'

# Qualify IPv6 loopback independently. Some managed Windows hosts disable the
# IPv6 protocol; skip only when the host itself cannot bind ::1.
$ipv6Listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::IPv6Loopback, 0)
$ipv6Available = $false
try {
  $ipv6Listener.Start()
  $ipv6Available = $true
} catch {
  Write-Output 'SKIP IPv6 loopback: host could not create an ::1 listener'
}
if ($ipv6Available) {
  try {
    foreach ($network in @('no','yes')) {
      $port = ([Net.IPEndPoint]$ipv6Listener.LocalEndpoint).Port
      Invoke-SandboxClientProbe $(if ($network -eq 'yes') {'network-allow'} else {'network-deny'}) $port '::1' $network
    }
  } finally {
    $ipv6Listener.Stop()
  }
  Write-Output 'PASS IPv6 loopback client policy with and without network capability'
}

# A remote private-LAN endpoint is optional because a fixture cannot assume a
# second machine or an open service. When supplied, first prove host reachability.
if ($PrivateLanAddress -or $PrivateLanPort) {
  if (!$PrivateLanAddress -or !$PrivateLanPort) {
    throw 'Supply both PrivateLanAddress and PrivateLanPort'
  }
  $lanAddress = [Net.IPAddress]::Parse($PrivateLanAddress)
  if ([Net.IPAddress]::IsLoopback($lanAddress)) {
    throw 'PrivateLanAddress must be a remote non-loopback address'
  }
  if (Test-HostTcp $lanAddress $PrivateLanPort) {
    Invoke-SandboxClientProbe 'network-deny' $PrivateLanPort $PrivateLanAddress 'no'
    Invoke-SandboxClientProbe 'network-allow' $PrivateLanPort $PrivateLanAddress 'yes'
    Write-Output 'PASS private-LAN client denied without capability and allowed with capability'
  } else {
    Write-Output 'SKIP private-LAN policy: host baseline could not connect to the supplied endpoint'
  }
} else {
  Write-Output 'SKIP private-LAN policy: supply a reachable remote endpoint to qualify it'
}

function Test-SandboxListener([bool]$Ipv6, [string]$Network) {
  $marker = Join-Path $workspace $(if ($Ipv6) { ('listener-v6-'+$Network+'.txt') } else { ('listener-v4-'+$Network+'.txt') })
  $operation = if ($Network -eq 'yes') {'network-listen-echo'} else {'network-listen'}
  $command = $operation+' "' + $marker + '" 5000'
  if ($Ipv6) { $command += ' ipv6' }
  $process = Start-SandboxRunner $command 'write' $Network 8000
  try {
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while (!(Test-Path -LiteralPath $marker)) {
      if ($process.HasExited -or [DateTime]::UtcNow -gt $deadline) {
        throw 'Sandboxed loopback listener did not report its bind result'
      }
      Start-Sleep -Milliseconds 25
    }
    $status = [IO.File]::ReadAllText($marker)
    if ($status -match '^BLOCKED (\d+)$') {
      if ($Network -eq 'yes' -or [int]$Matches[1] -ne 10013) {
        throw "Unexpected loopback bind error: $status"
      }
      Write-Output "PASS sandboxed loopback listener bind is denied (IPv6=$Ipv6)"
    } elseif ($status -match '^LISTEN(6)? (\d+)$') {
      $port = [int]$Matches[2]
      $family = if ($Ipv6) {
        [Net.Sockets.AddressFamily]::InterNetworkV6
      } else {
        [Net.Sockets.AddressFamily]::InterNetwork
      }
      $address = if ($Ipv6) { [Net.IPAddress]::IPv6Loopback } else { [Net.IPAddress]::Loopback }
      $client = [Net.Sockets.TcpClient]::new($family)
      try {
        $pending = $client.BeginConnect($address, $port, $null, $null)
        $connected = $false
        if ($pending.AsyncWaitHandle.WaitOne(2000)) {
          try { $client.EndConnect($pending); $connected=$true }
          catch [Net.Sockets.SocketException] { }
        }
        if ($connected -ne ($Network -eq 'yes')) { throw "Listener Network capability mismatch: $Network" }
        if ($connected) {
          $stream=$client.GetStream()
          $stream.ReadTimeout=3000
          $payload=[Text.Encoding]::ASCII.GetBytes('latch')
          $stream.Write($payload,0,$payload.Length)
          $reply=[byte[]]::new(5)
          $offset=0
          while($offset -lt 5){
            $count=$stream.Read($reply,$offset,5-$offset)
            if(!$count){throw 'Sandbox listener closed before replying'}
            $offset+=$count
          }
          if([Text.Encoding]::ASCII.GetString($reply) -ne 'latch'){throw 'Sandbox listener returned wrong payload'}
        }
      } finally {
        $client.Close()
      }
      Write-Output "PASS sandbox listener payload/policy (IPv6=$Ipv6 Network=$Network)"
    } else {
      throw "Unrecognized loopback listener status: $status"
    }
    if (!$process.WaitForExit(10000)) { throw 'Loopback listener runner did not exit' }
    if ($process.ExitCode) { throw "Loopback listener fixture exited $($process.ExitCode)" }
    if ((Get-Acl -LiteralPath $workspace).Sddl -ne $baseline) {
      throw 'Loopback fixture did not restore workspace ACL exactly'
    }
    if ((Get-Acl -LiteralPath $marker).Sddl -ne $templateAcl) {
      throw 'Loopback marker retained a temporary AppContainer grant'
    }
  } finally {
    if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
    $process.Dispose()
  }
}

foreach ($network in @('no','yes')) {
  Test-SandboxListener $false $network
  if ($ipv6Available) { Test-SandboxListener $true $network }
}
Write-Output 'PASS client and listening-server loopback policy'
$global:LASTEXITCODE = 0

# Test the capability protocol directly, bypassing compatibility hooks.
foreach($case in @(@('broker-socket-deny','no'),@('broker-raw-deny','yes'),@('broker-socket-caller-deny','yes'))) {
  & $runner $workspace $fixture ($case[0]+' "'+$workspace+'"') read --read-root $runtime --network $case[1] --timeout-ms 10000
  if($LASTEXITCODE){throw "Socket broker failed to reject $($case[0])"}
}
# Complete payload exchange with the sandbox as client (IPv4 and IPv6).
foreach($descendant in @($false,$true)) {
foreach($address in @([Net.IPAddress]::Loopback,$(if($ipv6Available){[Net.IPAddress]::IPv6Loopback}))) {
  if(!$address){continue}
  $listener=[Net.Sockets.TcpListener]::new($address,0)
  $listener.Start()
  $process=$null
  $client=$null
  try {
    $port=([Net.IPEndPoint]$listener.LocalEndpoint).Port
    $accept=$listener.AcceptTcpClientAsync()
    $command="network-echo $port $address"
    if($descendant){
      $cmd=Join-Path $env:SystemRoot 'System32/cmd.exe'
      $process=Start-SandboxRunner ('/d /s /c ""'+$fixture+'" '+$command+'"') 'read' 'yes' 10000 $cmd
    }else{$process=Start-SandboxRunner $command 'read' 'yes' 10000}
    if(!$accept.Wait(12000)){throw 'Sandbox echo client did not connect'}
    $client=$accept.GetAwaiter().GetResult()
    $stream=$client.GetStream()
    $stream.ReadTimeout=3000
    $payload=[byte[]]::new(5)
    $offset=0
    while($offset -lt 5){
      $count=$stream.Read($payload,$offset,5-$offset)
      if(!$count){throw 'Sandbox client closed before sending payload'}
      $offset+=$count
    }
    if([Text.Encoding]::ASCII.GetString($payload) -ne 'latch'){throw 'Sandbox client returned wrong payload'}
    $stream.Write($payload,0,5)
    $client.Close()
    if(!$process.WaitForExit(12000) -or $process.ExitCode){throw 'Sandbox echo client failed'}
    Write-Output "PASS sandbox TCP client payload exchange: $address descendant=$descendant"
  }finally{
    if($client){$client.Close()}
    $listener.Stop()
    if($process){if(!$process.HasExited){$process.Kill();$process.WaitForExit()};$process.Dispose()}
  }
}
}
foreach($address in @([Net.IPAddress]::Loopback,$(if($ipv6Available){[Net.IPAddress]::IPv6Loopback}))) {
 if(!$address){continue}
 foreach($network in @('no','yes')) {
  $udpHost=[Net.Sockets.UdpClient]::new($address.AddressFamily)
  $udpHost.Client.Bind([Net.IPEndPoint]::new($address,0))
  $process=$null
  try {
    $port=([Net.IPEndPoint]$udpHost.Client.LocalEndPoint).Port
    $receive=$udpHost.ReceiveAsync()
    $operation=if($network -eq 'yes'){'network-udp-echo'}else{'network-udp-deny'}
    $process=Start-SandboxRunner ("$operation $port $address") 'read' $network 10000
    $arrived=$receive.Wait(5000)
    if($arrived -ne ($network -eq 'yes')){throw "UDP capability mismatch: $address Network=$network"}
    if($arrived){
      $packet=$receive.GetAwaiter().GetResult()
      if([Text.Encoding]::ASCII.GetString($packet.Buffer) -ne 'latch'){throw 'Wrong UDP payload'}
      [void]$udpHost.Send($packet.Buffer,$packet.Buffer.Length,$packet.RemoteEndPoint)
    }
    if(!$process.WaitForExit(10000) -or $process.ExitCode){throw 'UDP sandbox fixture failed'}
    Write-Output "PASS UDP payload/policy: $address Network=$network"
  }finally{
    $udpHost.Dispose()
    if($process){if(!$process.HasExited){$process.Kill();$process.WaitForExit()};$process.Dispose()}
  }
 }
}
$global:LASTEXITCODE=0

# A killed launcher must not leave the transferred listener reachable or a
# host-held socket alive after the cleanup owner terminates its job.
$marker=Join-Path $workspace 'cancelled-listener.txt'
$process=Start-SandboxRunner ('network-listen "'+$marker+'" 15000') 'write' 'yes' 20000
try {
  $deadline=[DateTime]::UtcNow.AddSeconds(15)
  while(!(Test-Path -LiteralPath $marker)) {
    if($process.HasExited -or [DateTime]::UtcNow -gt $deadline){throw 'Cancellation listener did not start'}
    Start-Sleep -Milliseconds 25
  }
  $status=[IO.File]::ReadAllText($marker)
  if($status -notmatch '^LISTEN (\d+)$'){throw "Cancellation listener failed: $status"}
  $port=[int]$Matches[1]
  if(!(Test-HostTcp ([Net.IPAddress]::Loopback) $port)){throw 'Cancellation listener was never reachable'}
  $process.Kill()
  $process.WaitForExit()
  $deadline=[DateTime]::UtcNow.AddSeconds(15)
  do {
    $reachable=Test-HostTcp ([Net.IPAddress]::Loopback) $port
    if(!$reachable -and (Get-Acl -LiteralPath $workspace).Sddl -eq $baseline){break}
    if([DateTime]::UtcNow -gt $deadline){throw 'Cancelled socket or workspace grant survived cleanup'}
    Start-Sleep -Milliseconds 25
  } while($true)
  if((Get-Acl -LiteralPath $marker).Sddl -ne $templateAcl){throw 'Cancelled listener marker retained grants'}
  Write-Output 'PASS transferred listener closes and ACLs recover after launcher cancellation'
}finally{
  if(!$process.HasExited){$process.Kill();$process.WaitForExit()}
  $process.Dispose()
}
$global:LASTEXITCODE=0
