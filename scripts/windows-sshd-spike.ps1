# Temporary spike (deleted again once the Windows end-to-end server is settled): can
# Windows' OpenSSH server serve an SSH login as the runner's own user on GitHub's
# windows-latest runner? Tries an unprivileged `sshd.exe -D` first, then the installed
# sshd service, and prints `SPIKE unprivileged: ok|fail` and `SPIKE service: ok|fail`.
# Exits 0 if either works.
$ErrorActionPreference = 'Continue'
# Pass an empty argument (the empty passphrase) to native programs as an empty argument.
$PSNativeCommandArgumentPassing = 'Standard'

$work = Join-Path $env:RUNNER_TEMP 'sshd-spike'
New-Item -ItemType Directory -Force -Path $work | Out-Null
$user = $env:USERNAME
$sshdDir = 'C:\Windows\System32\OpenSSH'
$sshd = Join-Path $sshdDir 'sshd.exe'
$keygen = Join-Path $sshdDir 'ssh-keygen.exe'
$ssh = Join-Path $sshdDir 'ssh.exe'

function Fwd([string]$p) { $p -replace '\\', '/' }

function Restrict-Acl([string]$path) {
    & icacls $path /inheritance:r /grant:r "${user}:F" | Out-Null
}

function Get-FreePort {
    $l = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $l.Start()
    $port = $l.LocalEndpoint.Port
    $l.Stop()
    return $port
}

function Wait-Port([int]$port, [int]$seconds = 20) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline) {
        $c = [System.Net.Sockets.TcpClient]::new()
        try {
            $c.Connect('127.0.0.1', $port)
            return $true
        } catch {
            Start-Sleep -Milliseconds 300
        } finally {
            $c.Close()
        }
    }
    return $false
}

function New-Key([string]$path) {
    Remove-Item -Force -ErrorAction SilentlyContinue $path, "$path.pub"
    & $keygen -q -t ed25519 -N '' -f $path
    if ($LASTEXITCODE -ne 0) { throw "ssh-keygen failed for $path" }
}

function Try-Login([int]$port, [string]$key) {
    $known = Join-Path $work 'known_hosts'
    $out = & $ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o "UserKnownHostsFile=$known" `
        -o ConnectTimeout=15 -i $key -p $port "$user@127.0.0.1" echo ok 2>&1
    Write-Host "ssh exit=$LASTEXITCODE output=$out"
    return ($LASTEXITCODE -eq 0 -and ($out -join "`n") -match '(?m)^ok\s*$')
}

# Step 1: make sure sshd.exe exists.
if (-not (Test-Path $sshd)) {
    Write-Host 'sshd.exe missing: installing the OpenSSH Server capability'
    Add-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0' | Out-Host
}
if (-not (Test-Path $sshd)) {
    Write-Host 'capability did not provide sshd.exe: downloading the Win32-OpenSSH release'
    $zip = Join-Path $work 'openssh.zip'
    Invoke-WebRequest -UseBasicParsing -OutFile $zip `
        -Uri 'https://github.com/PowerShell/Win32-OpenSSH/releases/download/v9.8.1.0p1-Preview/OpenSSH-Win64.zip'
    Expand-Archive -Force -Path $zip -DestinationPath $work
    $sshdDir = Join-Path $work 'OpenSSH-Win64'
    $sshd = Join-Path $sshdDir 'sshd.exe'
    $keygen = Join-Path $sshdDir 'ssh-keygen.exe'
    $ssh = Join-Path $sshdDir 'ssh.exe'
}
Write-Host "sshd: $sshd"
& $sshd -V 2>&1 | Out-Host
Write-Host "user: $user ($(whoami))"

# Step 2: keys.
$clientKey = Join-Path $work 'client_key'
New-Key $clientKey
Restrict-Acl $clientKey
$pub = (Get-Content "$clientKey.pub" -Raw).Trim()

$results = @{ unprivileged = $false; service = $false }

# Mode 1: sshd.exe -D -e -f <config> as the runner user.
try {
    Write-Host '=== mode: unprivileged ==='
    $port = Get-FreePort
    $hostKey = Join-Path $work 'host_key'
    New-Key $hostKey
    Restrict-Acl $hostKey
    $authKeys = Join-Path $work 'authorized_keys'
    Set-Content -Path $authKeys -Value $pub -Encoding ascii
    Restrict-Acl $authKeys
    $config = Join-Path $work 'sshd_config'
    $pidFile = Join-Path $work 'sshd.pid'
    @"
Port $port
ListenAddress 127.0.0.1
HostKey $(Fwd $hostKey)
AuthorizedKeysFile $(Fwd $authKeys)
PidFile $(Fwd $pidFile)
StrictModes no
PubkeyAuthentication yes
PasswordAuthentication no
LogLevel DEBUG3
"@ | Set-Content -Path $config -Encoding ascii
    $errLog = Join-Path $work 'sshd-unprivileged.err'
    $outLog = Join-Path $work 'sshd-unprivileged.out'
    $proc = Start-Process -FilePath $sshd -ArgumentList '-D', '-e', '-f', $config `
        -RedirectStandardError $errLog -RedirectStandardOutput $outLog -PassThru -NoNewWindow
    if ((Wait-Port $port) -and (Try-Login $port $clientKey)) {
        $results.unprivileged = $true
    } else {
        Write-Host "sshd running: $(-not $proc.HasExited)"
    }
    if (-not $results.unprivileged) {
        Write-Host '--- sshd stderr ---'
        Get-Content $errLog -ErrorAction SilentlyContinue | Out-Host
        Write-Host '--- sshd stdout ---'
        Get-Content $outLog -ErrorAction SilentlyContinue | Out-Host
    }
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
} catch {
    Write-Host "unprivileged mode threw: $_"
}
Write-Host "SPIKE unprivileged: $(if ($results.unprivileged) { 'ok' } else { 'fail' })"

# Mode 2: the installed sshd service, on an extra port.
$svcConfig = 'C:\ProgramData\ssh\sshd_config'
$svcBackup = Join-Path $work 'sshd_config.orig'
try {
    Write-Host '=== mode: service ==='
    if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
        throw 'no sshd service installed'
    }
    # The first start creates C:\ProgramData\ssh, the host keys and the stock config.
    Start-Service sshd
    Stop-Service sshd
    Copy-Item -Force $svcConfig $svcBackup
    $port = Get-FreePort
    # Keywords before the stock config's Match blocks; the first value of a keyword wins.
    $stock = Get-Content $svcConfig -Raw
    @"
Port $port
ListenAddress 127.0.0.1
StrictModes no
PubkeyAuthentication yes
PasswordAuthentication no
LogLevel DEBUG3

"@ + $stock | Set-Content -Path $svcConfig -Encoding ascii
    # The runner user is an administrator: Win32-OpenSSH reads these keys from here.
    $adminKeys = 'C:\ProgramData\ssh\administrators_authorized_keys'
    Set-Content -Path $adminKeys -Value $pub -Encoding ascii
    & icacls $adminKeys /inheritance:r /grant:r 'Administrators:F' /grant:r 'SYSTEM:F' | Out-Null
    # Also in the user's own file, for the case the user is not treated as an administrator.
    $sshHome = Join-Path $env:USERPROFILE '.ssh'
    New-Item -ItemType Directory -Force -Path $sshHome | Out-Null
    $userKeys = Join-Path $sshHome 'authorized_keys'
    Set-Content -Path $userKeys -Value $pub -Encoding ascii
    Restrict-Acl $userKeys
    Start-Service sshd
    if ((Wait-Port $port) -and (Try-Login $port $clientKey)) {
        $results.service = $true
    }
    if (-not $results.service) {
        Write-Host '--- sshd service log (Application/OpenSSH) ---'
        Get-WinEvent -FilterHashtable @{ LogName = 'OpenSSH/Operational' } -MaxEvents 40 -ErrorAction SilentlyContinue |
            ForEach-Object { $_.Message } | Out-Host
        Get-ChildItem 'C:\ProgramData\ssh\logs' -ErrorAction SilentlyContinue | ForEach-Object {
            Write-Host "--- $($_.FullName) ---"
            Get-Content $_.FullName -Tail 80 | Out-Host
        }
    }
} catch {
    Write-Host "service mode threw: $_"
} finally {
    Stop-Service sshd -ErrorAction SilentlyContinue
    if (Test-Path $svcBackup) { Copy-Item -Force $svcBackup $svcConfig }
}
Write-Host "SPIKE service: $(if ($results.service) { 'ok' } else { 'fail' })"

if ($results.unprivileged -or $results.service) { exit 0 }
exit 1
