param(
    [string]$ServiceName = "NasFastCache",
    [string]$ConfigPath = "config\local.ps1",
    [string]$NssmPath = "nssm.exe",
    [string]$ServiceUser = "",
    [string]$ServicePassword = "",
    [string]$StdoutLog = "",
    [string]$StderrLog = "",
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$scriptPath = Join-Path $repoRoot "scripts\start-nas-fast-cache.ps1"
$resolvedConfig = if ([System.IO.Path]::IsPathRooted($ConfigPath)) {
    $ConfigPath
} else {
    Join-Path $repoRoot $ConfigPath
}

if (-not (Test-Path $scriptPath)) {
    throw "Mount script not found: $scriptPath"
}
if (-not (Test-Path $resolvedConfig)) {
    throw "Config file not found: $resolvedConfig"
}

if (($ServiceUser -and -not $ServicePassword) -or ($ServicePassword -and -not $ServiceUser)) {
    throw "ServiceUser and ServicePassword must be provided together."
}

$logDir = Join-Path $repoRoot "logs"
if (-not $StdoutLog) {
    $StdoutLog = Join-Path $logDir "$ServiceName.out.log"
}
if (-not $StderrLog) {
    $StderrLog = Join-Path $logDir "$ServiceName.err.log"
}
if (-not $DryRun) {
    New-Item -ItemType Directory -Force $logDir | Out-Null
}

$powershell = (Get-Command powershell.exe -ErrorAction Stop).Source
$arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$scriptPath`" -ConfigPath `"$resolvedConfig`""
$commands = @(
    @("install", $ServiceName, $powershell, $arguments),
    @("set", $ServiceName, "AppDirectory", $repoRoot),
    @("set", $ServiceName, "DisplayName", "NAS Fast Cache"),
    @("set", $ServiceName, "Description", "WinFsp read-through cache mount for a NAS-backed path."),
    @("set", $ServiceName, "Start", "SERVICE_AUTO_START"),
    @("set", $ServiceName, "AppStdout", $StdoutLog),
    @("set", $ServiceName, "AppStderr", $StderrLog),
    @("set", $ServiceName, "AppRotateFiles", "1"),
    @("set", $ServiceName, "AppRotateOnline", "1"),
    @("set", $ServiceName, "AppRotateBytes", "10485760"),
    @("set", $ServiceName, "AppExit", "Default", "Restart"),
    @("set", $ServiceName, "AppRestartDelay", "5000"),
    @("set", $ServiceName, "AppThrottle", "1500")
)

if ($ServiceUser) {
    $commands += ,@("set", $ServiceName, "ObjectName", $ServiceUser, $ServicePassword)
}

foreach ($command in $commands) {
    $line = @($NssmPath) + $command
    if ($DryRun) {
        $line -join " "
    } else {
        & $NssmPath @command
        if ($LASTEXITCODE -ne 0) {
            throw "nssm command failed: $($line -join ' ')"
        }
    }
}

if (-not $DryRun) {
    "installed service=$ServiceName"
}
