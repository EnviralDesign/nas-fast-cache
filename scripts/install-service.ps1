param(
    [string]$ServiceName = "NasFastCache",
    [string]$ConfigPath = "config\local.ps1",
    [string]$NssmPath = "nssm.exe",
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

$powershell = (Get-Command powershell.exe -ErrorAction Stop).Source
$arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$scriptPath`" -ConfigPath `"$resolvedConfig`""
$commands = @(
    @("install", $ServiceName, $powershell, $arguments),
    @("set", $ServiceName, "AppDirectory", $repoRoot),
    @("set", $ServiceName, "DisplayName", "NAS Fast Cache"),
    @("set", $ServiceName, "Description", "WinFsp read-through cache mount for a NAS-backed path."),
    @("set", $ServiceName, "Start", "SERVICE_AUTO_START")
)

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
