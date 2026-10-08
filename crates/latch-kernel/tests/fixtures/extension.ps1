$ErrorActionPreference = 'Stop'
$script:inputStream = [Console]::OpenStandardInput()
$script:outputStream = [Console]::OpenStandardOutput()
$script:utf8 = [System.Text.UTF8Encoding]::new($false)

function Read-HeaderLine {
  $line = [System.Text.StringBuilder]::new()
  while ($true) {
    $value = $script:inputStream.ReadByte()
    if ($value -lt 0) {
      if ($line.Length -eq 0) { return $null }
      return $line.ToString()
    }
    if ($value -eq 10) { return $line.ToString() }
    if ($value -ne 13) { [void]$line.Append([char]$value) }
  }
}

function Read-Message {
  $contentLength = -1
  while ($true) {
    $line = Read-HeaderLine
    if ($null -eq $line) { return $null }
    if ($line.Length -eq 0) { break }
    if ($line -match '^Content-Length:\s*(\d+)$') {
      $contentLength = [int]$Matches[1]
    }
  }
  if ($contentLength -lt 0) { throw 'Missing Content-Length header' }

  $body = New-Object byte[] $contentLength
  $offset = 0
  while ($offset -lt $body.Length) {
    $read = $script:inputStream.Read($body, $offset, $body.Length - $offset)
    if ($read -le 0) { throw 'Unexpected end of extension input' }
    $offset += $read
  }
  $json = $script:utf8.GetString($body)
  return (ConvertFrom-Json -InputObject $json)
}

function Send-Message {
  param([Parameter(Mandatory = $true)][hashtable]$Message)
  $json = ConvertTo-Json -InputObject $Message -Depth 32 -Compress
  $body = $script:utf8.GetBytes($json)
  $header = [System.Text.Encoding]::ASCII.GetBytes(
    "Content-Length: $($body.Length)`r`n`r`n")
  $script:outputStream.Write($header, 0, $header.Length)
  $script:outputStream.Write($body, 0, $body.Length)
  $script:outputStream.Flush()
}

while ($true) {
  $message = Read-Message
  if ($null -eq $message) { break }
  switch ([string]$message.method) {
    'initialize' {
      Send-Message -Message @{
        jsonrpc = '2.0'; id = $message.id
        result = @{ protocolVersion = '0.1' }
      }
    }
    'initialized' {
      Send-Message -Message @{
        jsonrpc = '2.0'; id = 101; method = 'tool.register'
        params = @{ name = 'fixture.echo'; description = 'echo'; inputSchema = @{ type = 'object' } }
      }
      Send-Message -Message @{
        jsonrpc = '2.0'; id = 102; method = 'command.register'
        params = @{ name = 'fixture-about' }
      }
      Send-Message -Message @{
        jsonrpc = '2.0'; id = 103; method = 'hook.guard'
        params = @{ action = 'tool.execute' }
      }
      Send-Message -Message @{
        jsonrpc = '2.0'; id = 104; method = 'hook.transform'
        params = @{ structure = 'model_request' }
      }
      Send-Message -Message @{
        jsonrpc = '2.0'; id = 105; method = 'context_source.register'
        params = @{ name = 'fixture.context' }
      }
      Send-Message -Message @{
        jsonrpc = '2.0'; method = 'ready'; params = @{}
      }
    }
    'tool.execute' {
      Send-Message -Message @{
        jsonrpc = '2.0'; id = $message.id
        result = @{ echoed = $message.params.arguments.value }
      }
    }
    'hook.guard' {
      Send-Message -Message @{
        jsonrpc = '2.0'; id = $message.id; result = @{ decision = 'allow' }
      }
    }
    'hook.transform' {
      Send-Message -Message @{
        jsonrpc = '2.0'; id = $message.id; result = $message.params.value
      }
    }
    'context_source.get' {
      Send-Message -Message @{
        jsonrpc = '2.0'; id = $message.id
        result = @{ name = 'fixture.context'; content = 'fixture context' }
      }
    }
    'shutdown' {
      Send-Message -Message @{
        jsonrpc = '2.0'; id = $message.id; result = $null
      }
    }
    'exit' { break }
  }
  if ([string]$message.method -eq 'exit') { break }
}
