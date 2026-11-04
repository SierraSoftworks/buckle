# Installs Buckle on Windows, optionally joins the tailnet first, then runs a
# Buckle command (usually `up`, which bootstraps the agent). From an elevated
# PowerShell:
#
#   & ([scriptblock]::Create((irm https://buckle.sierrasoftworks.com/install.ps1))) `
#     [-Version 2.4.0] [-TailscaleAuthKey tskey-…] up --server … --label role=worker
#
# Options (before the Buckle command):
#   -Version VERSION          Release to install (default: the latest release).
#   -InstallDir DIR           Where to put buckle.exe (default: $env:ProgramFiles\Buckle).
#   -TailscaleAuthKey KEY     Join the tailnet with this auth key before running
#                             Buckle, installing Tailscale first. Also read from
#                             $env:TS_AUTHKEY.
#   -TailscaleHostname NAME   The node name to join as.
#   -TailscaleTags TAGS       Comma-separated ACL tags to advertise (tag:a,tag:b).
#   -TailscaleArg ARG         Another argument for `tailscale up` (repeatable).
#
# Everything after the options is passed to buckle.exe. The binary is verified
# against the SHA-256 digest GitHub reports for the release asset.
#
# Arguments are parsed by hand (there is no param block) so that the Buckle
# command's own `--flags` reach buckle.exe untouched.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = 'SierraSoftworks/buckle'
$Version = $env:BUCKLE_VERSION
$InstallDir = if ($env:BUCKLE_INSTALL_DIR) { $env:BUCKLE_INSTALL_DIR } else { Join-Path $env:ProgramFiles 'Buckle' }
$TsKey = $env:TS_AUTHKEY
$TsHostname = $null
$TsTags = $null
$TsArgs = @()

function Say([string]$Message) { [Console]::Error.WriteLine("buckle-install: $Message") }
# Throws rather than exits: this runs inside the caller's session.
function Fail([string]$Message) { throw "buckle-install: $Message" }

$rest = @($args)
$options = '-Version', '-InstallDir', '-TailscaleAuthKey', '-TailscaleHostname', '-TailscaleTags', '-TailscaleArg'
$i = 0
while ($i -lt $rest.Count) {
    $name = [string]$rest[$i]
    if ($name -eq '--') { $i += 1; break }
    if ($options -notcontains $name) { break }
    if ($i + 1 -ge $rest.Count) { Fail "$name needs a value" }
    $value = [string]$rest[$i + 1]
    $i += 2
    switch ($name) {
        '-Version' { $Version = $value }
        '-InstallDir' { $InstallDir = $value }
        '-TailscaleAuthKey' { $TsKey = $value }
        '-TailscaleHostname' { $TsHostname = $value }
        '-TailscaleTags' { $TsTags = $value }
        '-TailscaleArg' { $TsArgs += $value }
    }
}
$BuckleArgs = if ($i -lt $rest.Count) { @($rest[$i..($rest.Count - 1)]) } else { @() }

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Fail 'run from an elevated (Administrator) PowerShell.'
}
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64') { Fail "unsupported architecture $arch; Buckle ships buckle-windows-amd64.exe only." }
$Asset = 'buckle-windows-amd64.exe'

$headers = @{ Accept = 'application/vnd.github+json'; 'User-Agent' = 'buckle-install' }
if ($env:GITHUB_TOKEN) { $headers.Authorization = "Bearer $env:GITHUB_TOKEN" }

# --- Tailscale ---------------------------------------------------------------

function Find-Tailscale {
    $cmd = Get-Command tailscale -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $default = Join-Path $env:ProgramFiles 'Tailscale\tailscale.exe'
    if (Test-Path $default) { return $default }
    return $null
}

function Test-Tailnet([string]$Ts) {
    & $Ts ip -4 *> $null
    return $LASTEXITCODE -eq 0
}

if ($TsKey) {
    $ts = Find-Tailscale
    if (-not $ts) {
        Say 'installing Tailscale'
        $msi = Join-Path ([IO.Path]::GetTempPath()) 'tailscale-setup.msi'
        Invoke-WebRequest -UseBasicParsing -Uri 'https://pkgs.tailscale.com/stable/tailscale-setup-latest-amd64.msi' -OutFile $msi
        $p = Start-Process msiexec.exe -ArgumentList @('/i', "`"$msi`"", '/quiet', '/norestart') -Wait -PassThru
        Remove-Item $msi -Force -ErrorAction SilentlyContinue
        if ($p.ExitCode -ne 0) { Fail "the Tailscale installer exited with $($p.ExitCode)." }
        $ts = Find-Tailscale
        if (-not $ts) { Fail 'Tailscale was installed but tailscale.exe was not found.' }
    }
    if (Test-Tailnet $ts) {
        Say 'already on the tailnet; skipping tailscale up'
    } else {
        $up = @('up', '--unattended', "--auth-key=$TsKey")
        if ($TsHostname) { $up += "--hostname=$TsHostname" }
        if ($TsTags) { $up += "--advertise-tags=$TsTags" }
        $up += $TsArgs
        Say 'joining the tailnet'
        & $ts @up
        if ($LASTEXITCODE -ne 0) { Fail "tailscale up exited with $LASTEXITCODE." }
        $n = 0
        while (-not (Test-Tailnet $ts)) {
            $n += 1
            if ($n -gt 30) { Fail 'the node did not get a tailnet address.' }
            Start-Sleep -Seconds 1
        }
    }
}

# --- Buckle ------------------------------------------------------------------

$api = if ($Version) { "https://api.github.com/repos/$Repo/releases/tags/v$($Version.TrimStart('v'))" } else { "https://api.github.com/repos/$Repo/releases/latest" }
try { $release = Invoke-RestMethod -UseBasicParsing -Headers $headers -Uri $api } catch { Fail "could not read the release from ${api}: $_" }
$tag = $release.tag_name
$entry = $release.assets | Where-Object { $_.name -eq $Asset } | Select-Object -First 1
if (-not $entry -or -not $entry.digest) { Fail "release $tag has no verified $Asset; pick another -Version." }
$digest = ($entry.digest -replace '^sha256:', '').ToLowerInvariant()

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$target = Join-Path $InstallDir 'buckle.exe'
$download = Join-Path $InstallDir ".buckle-install-$PID.exe"
Say "downloading $Asset $tag"
try {
    Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/$Repo/releases/download/$tag/$Asset" -OutFile $download
    $actual = (Get-FileHash -Algorithm SHA256 -Path $download).Hash.ToLowerInvariant()
    if ($actual -ne $digest) { Fail "$Asset does not match its published digest." }
    # A running agent holds buckle.exe open; Windows lets it be renamed aside.
    if (Test-Path $target) {
        $old = "$target.old"
        Remove-Item $old -Force -ErrorAction SilentlyContinue
        Move-Item -Force $target $old
    }
    Move-Item -Force $download $target
} finally {
    Remove-Item $download -Force -ErrorAction SilentlyContinue
}

# Machine PATH, so `buckle` works in new shells.
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (-not (($machinePath -split ';') -contains $InstallDir)) {
    [Environment]::SetEnvironmentVariable('Path', "$machinePath;$InstallDir", 'Machine')
}
$env:Path = "$env:Path;$InstallDir"
Say "installed $(& $target --version) at $target"

if ($BuckleArgs.Count -gt 0) {
    & $target @BuckleArgs
    if ($LASTEXITCODE -ne 0) { Fail "buckle $($BuckleArgs[0]) exited with $LASTEXITCODE." }
}
