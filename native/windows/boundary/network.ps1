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
                             [string]$Network, [int]$TimeoutMs) {
  $info = New-Object Diagnostics.ProcessStartInfo
  $info.FileName = $runner
  $info.UseShellExecute = $false
  $info.CreateNoWindow = $true
  $escaped = $Command.Replace('"','\"')
  $info.Arguments = $q+$workspace+$q+' '+$q+$fixture+$q+' '+$q+$escaped+$q+
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
  $hostListener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
  $hostListener.Start()
  try {
    $port = ([Net.IPEndPoint]$hostListener.LocalEndpoint).Port
    Invoke-SandboxClientProbe 'network-deny' $port 'localhost' $network
    Invoke-SandboxClientProbe 'network-deny' $port '127.0.0.1' $network
  } finally {
    $hostListener.Stop()
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
      Invoke-SandboxClientProbe 'network-deny' $port '::1' $network
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

function Test-SandboxListener([bool]$Ipv6) {
  $marker = Join-Path $workspace $(if ($Ipv6) { 'listener-v6.txt' } else { 'listener-v4.txt' })
  $command = 'network-listen "' + $marker + '" 5000'
  if ($Ipv6) { $command += ' ipv6' }
  $process = Start-SandboxRunner $command 'write' 'yes' 8000
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
      if ([int]$Matches[1] -ne 10013) {
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
        if ($pending.AsyncWaitHandle.WaitOne(2000)) {
          try {
            $client.EndConnect($pending)
            throw 'Host connected to sandboxed loopback listener without an explicit exemption'
          } catch [Net.Sockets.SocketException] {
            # A reset/refusal is also a denied inbound loopback connection.
          }
        }
      } finally {
        $client.Close()
      }
      Write-Output "PASS host cannot reach sandboxed loopback listener (IPv6=$Ipv6)"
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

Test-SandboxListener $false
if ($ipv6Available) { Test-SandboxListener $true }
Write-Output 'PASS client and listening-server loopback policy'
$global:LASTEXITCODE = 0
