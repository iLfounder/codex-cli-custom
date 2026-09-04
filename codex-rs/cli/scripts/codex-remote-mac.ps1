[CmdletBinding()]
param(
    [ValidatePattern('^[A-Za-z0-9._@-]+$')]
    [string]$SshAlias = 'mac',

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^/[^\s"'']+$')]
    [string]$RemoteSocket,

    [ValidateRange(1, 65535)]
    [int]$LocalPort = 47771,

    [string]$Cwd,

    [ValidateRange(1, 120)]
    [int]$ReadyTimeoutSeconds = 15,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CodexArguments
)

$ErrorActionPreference = 'Stop'

$codexExe = Join-Path -Path $PSScriptRoot -ChildPath 'codex.exe'
if (-not (Test-Path -LiteralPath $codexExe -PathType Leaf)) {
    throw "Sibling codex.exe was not found at '$codexExe'."
}

$sshCommand = Get-Command -Name 'ssh.exe' -CommandType Application -ErrorAction Stop
$loopback = [System.Net.IPAddress]::Loopback
$portProbe = [System.Net.Sockets.TcpListener]::new($loopback, $LocalPort)
$portProbe.Server.ExclusiveAddressUse = $true
try {
    $portProbe.Start()
}
catch {
    throw "Loopback port $LocalPort is already occupied. Select a free -LocalPort; an existing listener is never reused."
}
finally {
    $portProbe.Stop()
}

$forward = "127.0.0.1:${LocalPort}:$RemoteSocket"
$sshArguments = @(
    '-N',
    '-o', 'ExitOnForwardFailure=yes',
    '-o', 'ServerAliveInterval=15',
    '-o', 'ServerAliveCountMax=3',
    '-L', $forward,
    $SshAlias
)

$sshProcess = Start-Process `
    -FilePath $sshCommand.Source `
    -ArgumentList $sshArguments `
    -PassThru `
    -WindowStyle Hidden
$sshStartedAt = $sshProcess.StartTime.ToUniversalTime()

function Test-OwnedSshProcess {
    param(
        [Parameter(Mandatory = $true)]
        [System.Diagnostics.Process]$ExpectedProcess,

        [Parameter(Mandatory = $true)]
        [datetime]$ExpectedStartTime
    )

    $actual = Get-Process -Id $ExpectedProcess.Id -ErrorAction SilentlyContinue
    if ($null -eq $actual) {
        return $false
    }
    return $actual.StartTime.ToUniversalTime() -eq $ExpectedStartTime
}

$exitCode = 1
try {
    $deadline = [DateTime]::UtcNow.AddSeconds($ReadyTimeoutSeconds)
    $ready = $false
    while ([DateTime]::UtcNow -lt $deadline) {
        $sshProcess.Refresh()
        if ($sshProcess.HasExited) {
            throw "SSH tunnel exited before the remote app-server became ready (exit code $($sshProcess.ExitCode))."
        }

        $client = [System.Net.Sockets.TcpClient]::new()
        try {
            $connect = $client.ConnectAsync($loopback, $LocalPort)
            if ($connect.Wait(250) -and $client.Connected) {
                $ready = $true
                break
            }
        }
        catch {
            # The forward is still starting or its remote Unix socket is not ready.
        }
        finally {
            $client.Dispose()
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) {
        throw "Timed out waiting $ReadyTimeoutSeconds seconds for ws://127.0.0.1:$LocalPort."
    }

    $arguments = @('--remote', "ws://127.0.0.1:$LocalPort")
    if (-not [string]::IsNullOrWhiteSpace($Cwd)) {
        $arguments += @('-C', $Cwd)
    }
    if ($null -ne $CodexArguments) {
        $arguments += $CodexArguments
    }

    & $codexExe @arguments
    $exitCode = $LASTEXITCODE
}
finally {
    if (Test-OwnedSshProcess -ExpectedProcess $sshProcess -ExpectedStartTime $sshStartedAt) {
        Stop-Process -Id $sshProcess.Id -ErrorAction SilentlyContinue
        $sshProcess.WaitForExit(5000) | Out-Null
    }
}

exit $exitCode
